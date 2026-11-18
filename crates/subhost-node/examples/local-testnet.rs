//! Four loopback validators with distinct keys and one shared genesis.
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;
use subhost_core::{
    Address, GenesisConfig, Transaction, TransactionSignature, TransactionType, ValidatorInfo,
};
use subhost_crypto::BLSScheme;
use subhost_node::{
    write_producer_key, Node, NodeConfig, P2pConfig, GENESIS_FILE_NAME, PRODUCER_KEY_FILE_NAME,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

struct Options {
    directory: PathBuf,
    rpc_port: u16,
    p2p_port: u16,
    smoke: bool,
}

fn options() -> Result<Options> {
    let mut options = Options {
        directory: std::env::temp_dir().join("subhost-four-validators"),
        rpc_port: 18545,
        p2p_port: 30340,
        smoke: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => options.directory = args.next().ok_or("missing --data-dir value")?.into(),
            "--rpc-base-port" => options.rpc_port = args.next().ok_or("missing RPC port")?.parse()?,
            "--p2p-base-port" => options.p2p_port = args.next().ok_or("missing P2P port")?.parse()?,
            "--smoke" => options.smoke = true,
            _ => return Err(format!("unknown argument: {arg}; use --data-dir, --rpc-base-port, --p2p-base-port or --smoke").into()),
        }
    }
    for port in [options.rpc_port, options.p2p_port] {
        if port == 0 || port.checked_add(3).is_none() {
            return Err("port ranges must fit four nonzero ports".into());
        }
    }
    if options.rpc_port.abs_diff(options.p2p_port) < 4 {
        return Err("RPC and P2P port ranges overlap".into());
    }
    Ok(options)
}

fn development_account() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

fn prepare(directory: &Path) -> Result<GenesisConfig> {
    let shared = directory.join(GENESIS_FILE_NAME);
    let genesis = if directory.exists() {
        GenesisConfig::load(&shared).map_err(|_| "existing testnet directory needs a valid shared genesis; use a new directory for an incomplete setup")?
    } else {
        std::fs::create_dir_all(directory)?;
        let key = development_account();
        let sender = Address::from_public_key(&key.verifying_key().to_bytes());
        let mut genesis = GenesisConfig { chain_id: 1337, ..Default::default() };
        genesis.allocations.insert(sender, 1_000_000_000);
        for index in 0..4u8 {
            let (secret, public) = BLSScheme::keygen();
            let node_dir = directory.join(format!("validator-{}", index + 1));
            std::fs::create_dir(&node_dir)?;
            write_producer_key(&node_dir.join(PRODUCER_KEY_FILE_NAME), &secret)?;
            genesis.initial_validators.push(ValidatorInfo {
                address: Address::new([0x11 + index; 20]),
                public_key: BLSScheme::encode_public_key(&public),
                power: 100,
                proof_of_possession: BLSScheme::encode_signature(&BLSScheme::proof_of_possession(
                    &secret,
                )),
            });
        }
        genesis.save(&shared)?;
        genesis
    };
    if genesis.chain_id != 1337 || genesis.initial_validators.len() != 4 {
        return Err("this example requires its four-validator chain-1337 genesis".into());
    }
    for index in 1..=4 {
        let path = directory.join(format!("validator-{index}")).join(GENESIS_FILE_NAME);
        if path.exists() {
            if serde_json::to_value(GenesisConfig::load(&path)?)? != serde_json::to_value(&genesis)?
            {
                return Err(format!("validator {index} has a different genesis").into());
            }
        } else {
            genesis.save(&path)?;
        }
    }
    Ok(genesis)
}

async fn rpc(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Result<Value> {
    let response: Value = client
        .post(url)
        .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if let Some(error) = response.get("error") {
        return Err(format!("{method}: {error}").into());
    }
    response.get("result").cloned().ok_or_else(|| "missing RPC result".into())
}

async fn wait_for_rpc(client: &reqwest::Client, urls: &[String]) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let mut ready = true;
            for url in urls {
                if rpc(client, url, "eth_chainId", json!([])).await.ok() != Some(json!("0x539")) {
                    ready = false;
                }
            }
            if ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| "four validator RPC servers did not become ready".into())
}

async fn smoke(client: &reqwest::Client, urls: &[String], chain_id: u64) -> Result<()> {
    let key = development_account();
    let public_key = key.verifying_key().to_bytes();
    let sender = Address::from_public_key(&public_key);
    for value in [10u128, 20] {
        let nonce =
            rpc(client, &urls[0], "eth_getTransactionCount", json!([sender.to_string(), "latest"]))
                .await?;
        let nonce = u64::from_str_radix(
            nonce.as_str().ok_or("invalid nonce")?.trim_start_matches("0x"),
            16,
        )?;
        let mut tx = Transaction {
            chain_id,
            nonce,
            from: sender,
            to: Some(Address::new([2; 20])),
            value,
            gas_limit: 21_000,
            gas_price: 1,
            data: Vec::new(),
            tx_type: TransactionType::Transfer,
            public_key,
            signature: TransactionSignature::EMPTY,
        };
        tx.signature =
            TransactionSignature::from_ed25519(&key.sign(&tx.signing_payload()).to_bytes());
        let hash = rpc(client, &urls[0], "eth_sendTransaction", json!([{
            "from":sender.to_string(), "to":tx.to.unwrap().to_string(), "value":format!("0x{value:x}"),
            "nonce":format!("0x{nonce:x}"), "gas":"0x5208", "gasPrice":"0x1", "chainId":format!("0x{chain_id:x}"),
            "publicKey":format!("0x{}", hex::encode(public_key)),
            "signature":format!("0x{}", hex::encode(tx.signature.to_ed25519())),
        }])).await?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let mut complete = true;
                for url in urls {
                    if rpc(client, url, "eth_getTransactionReceipt", json!([hash.clone()]))
                        .await?
                        .is_null()
                    {
                        complete = false;
                    }
                }
                if complete {
                    return Ok::<_, Box<dyn Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "receipt did not reach all four validators")??;
        println!("Certified transaction {hash} is visible on all four validators");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = options()?;
    let genesis = prepare(&options.directory)?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).build()?;
    let mut tasks = tokio::task::JoinSet::new();
    let mut states = Vec::new();
    let mut urls = Vec::new();
    // A star makes bootstrap retry observable; every follower knows the hub's
    // endpoint, and each validator keeps its own signing key and ledger.
    for index in 0..4u16 {
        let rpc_port = options.rpc_port + index;
        let p2p_port = options.p2p_port + index;
        let mut config =
            NodeConfig::new(options.directory.join(format!("validator-{}", index + 1)));
        config.validator = true;
        config.rpc_addr = ([127, 0, 0, 1], rpc_port).into();
        config.network = Some(P2pConfig {
            listen_addr: format!("/ip4/127.0.0.1/tcp/{p2p_port}"),
            bootstrap_peers: if index == 0 {
                Vec::new()
            } else {
                vec![format!("/ip4/127.0.0.1/tcp/{}", options.p2p_port)]
            },
            enable_mdns: false,
            ..Default::default()
        });
        let node = Node::bootstrap(config)?;
        states.push(node.state().clone());
        urls.push(format!("http://127.0.0.1:{rpc_port}"));
        tasks.spawn(node.run());
        println!("Validator {}: RPC {}, P2P 127.0.0.1:{p2p_port}", index + 1, urls.last().unwrap());
    }
    tokio::select! {
        result = wait_for_rpc(&client, &urls) => result?,
        result = tasks.join_next() => return Err(format!("validator stopped during startup: {result:?}").into()),
    }
    println!("Local testnet ready: chain 1337, quorum 3/4, data {}", options.directory.display());
    if options.smoke {
        smoke(&client, &urls, genesis.chain_id).await?;
        let first = &states[0];
        if states.iter().any(|state| {
            state.tip_hash() != first.tip_hash() || state.state_root() != first.state_root()
        }) {
            return Err("validator tips or state roots diverged".into());
        }
        println!(
            "Smoke passed: four equal tips and state roots at height {}",
            first.block_height()
        );
        tasks.shutdown().await;
    } else {
        println!("Press Ctrl-C to stop all four validators.");
        if let Some(result) = tasks.join_next().await {
            result??;
        }
        tasks.shutdown().await;
    }
    Ok(())
}
