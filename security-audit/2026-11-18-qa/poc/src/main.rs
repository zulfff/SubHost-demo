use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use subhost_consensus::{ConsensusConfig, HotStuff, QuorumCertificate, ValidatorRegistry};
use subhost_core::{Address, Hash, Transaction, TransactionSignature, TransactionType};
use subhost_crypto::BLSScheme;
use subhost_faucet::{FaucetConfig, FaucetSigner, FaucetState};
use subhost_network::{NetworkMessage, MAX_TRANSMIT_BYTES};
use subhost_rpc::{BlockAuthority, RpcServer, RpcServerConfig, RpcState};
use subhost_wallet::Wallet;

async fn call(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Value {
    client.post(url).json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send().await.unwrap().json().await.unwrap()
}

#[tokio::main]
async fn main() {
    // Synthetic keys only. No producer secrets or wallet contents are printed.
    let secret = BLSScheme::keygen().0;
    let mut aliases = ValidatorRegistry::new();
    for i in 1..=3 { aliases.register(Address::new([i;20]), BLSScheme::public_key(&secret), &BLSScheme::proof_of_possession(&secret)).unwrap(); }
    let fourth = BLSScheme::keygen().0;
    aliases.register(Address::new([4;20]), BLSScheme::public_key(&fourth), &BLSScheme::proof_of_possession(&fourth)).unwrap();
    let block_hash = Hash::from_data(b"qa synthetic block");
    let signature = BLSScheme::encode_signature(&BLSScheme::sign(&secret, &QuorumCertificate::payload_for(1,&block_hash)));
    let qc = QuorumCertificate { view:1, block_hash, signatures:(1..=3).map(|i|(Address::new([i;20]),signature.clone())).collect() };
    let accepted = HotStuff::new(ConsensusConfig::new(4).unwrap(), aliases).validate_qc(&qc).is_ok();
    println!("{}", json!({"case":"duplicate_bls_key_quorum","logical_validators":4,"distinct_signing_keys":1,"quorum_signatures":3,"accepted":accepted}));
    assert!(accepted, "QA probe records whether the unsafe configuration is accepted");

    let mut registry = ValidatorRegistry::new();
    let keys: Vec<_> = (1..=4).map(|_|BLSScheme::keygen().0).collect();
    for (i,key) in keys.iter().enumerate() { registry.register(Address::new([i as u8+1;20]), BLSScheme::public_key(key), &BLSScheme::proof_of_possession(key)).unwrap(); }
    let state = RpcState::in_memory(1).unwrap().with_authority(BlockAuthority::new(registry,Address::new([1;20]),keys[0].clone()).unwrap());
    let tx_key = SigningKey::from_bytes(&[7;32]);
    let public_key = tx_key.verifying_key().to_bytes();
    let sender = Address::from_public_key(&public_key);
    let dir = tempfile::tempdir().unwrap();
    let wallet_path = dir.path().join("synthetic-wallet.json");
    Wallet::new("qa-only-generated-wallet").unwrap().save(&wallet_path).unwrap();
    let faucet_signer = FaucetSigner::load(&wallet_path,"qa-only-generated-wallet").unwrap();
    state.seed_accounts([(sender,10_000_000),(faucet_signer.address(),10_000_000)]).unwrap();
    let (addr,rpc_handle) = RpcServer::new(state.clone()).start(RpcServerConfig { listen_addr:"127.0.0.1:0".parse().unwrap(),..Default::default() }).await.unwrap();
    let url=format!("http://{addr}");
    let client=reqwest::Client::new();
    let mut tx=Transaction { tx_type:TransactionType::Transfer, nonce:0, from:sender,to:Some(Address::new([9;20])),value:1,gas_price:1,gas_limit:21_000,data:vec![0xff;120*1024],chain_id:1,public_key,signature:TransactionSignature::EMPTY };
    tx.signature=TransactionSignature::from_ed25519(&tx_key.sign(&tx.signing_payload()).to_bytes());
    let params=json!([{"from":tx.from.to_string(),"to":tx.to.unwrap().to_string(),"nonce":"0x0","value":"0x1","gas":"0x5208","gasPrice":"0x1","chainId":"0x1","data":format!("0x{}",hex::encode(&tx.data)),"publicKey":format!("0x{}",hex::encode(tx.public_key)),"signature":format!("0x{}",hex::encode(tx.signature.to_ed25519()))}]);
    let rpc_size=serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":"eth_sendTransaction","params":params})).unwrap().len();
    let response=call(&client,&url,"eth_sendTransaction",params).await;
    assert!(response.get("result").is_some(),"large transfer must actually be admitted over HTTP: {response}");
    let block=state.build_block().unwrap().unwrap();
    let data=subhost_core::encode_canonical(&block);
    let raw_size=data.len();
    let message=NetworkMessage::BlockProposal {view:1,data};
    let wire_size=serde_json::to_vec(&message).unwrap().len();
    println!("{}",json!({"case":"admitted_block_exceeds_gossip_wire_limit","rpc_request_bytes":rpc_size,"raw_block_bytes":raw_size,"encoded_gossip_bytes":wire_size,"gossip_cap_bytes":MAX_TRANSMIT_BYTES,"raw_validator_accepts":message.validate().is_ok(),"rpc_admitted":true}));
    assert!(message.validate().is_ok() && wire_size>MAX_TRANSMIT_BYTES);

    let faucet=FaucetState::new(FaucetConfig {rpc_url:url.clone(),drip_amount:1,cooldown_secs:60,..Default::default()},faucet_signer.clone()).unwrap();
    let first=faucet.drip(&Address::new([10;20]).to_string()).await;
    let second=faucet.drip(&Address::new([11;20]).to_string()).await;
    let nonce=call(&client,&url,"eth_getTransactionCount",json!([faucet_signer.address().to_string(),"pending"])).await;
    println!("{}",json!({"case":"faucet_reuses_committed_nonce_before_quorum","first_ok":first.is_ok(),"second_ok":second.is_ok(),"second_error":second.err().map(|error|error.to_string()),"pending_nonce_result":nonce.get("result"),"height":state.block_height()}));
    assert!(first.is_ok());
    rpc_handle.stop().unwrap();
    rpc_handle.stopped().await;

    // Control: the HTTP faucet route normalizes whitespace; it is not a remote bypass.
    let sends=Arc::new(AtomicUsize::new(0));
    let observed=sends.clone();
    let mock=axum::Router::new().route("/",axum::routing::post(move |axum::Json(body):axum::Json<Value>|{
        let observed=observed.clone(); async move {
            let result=match body["method"].as_str().unwrap() {"eth_chainId"=>json!("0x1"),"eth_getTransactionCount"=>json!("0x0"),"eth_sendTransaction"=>{observed.fetch_add(1,Ordering::SeqCst);json!(format!("0x{}","aa".repeat(32)))},other=>panic!("unexpected mock method {other}")};
            axum::Json(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
        }
    }));
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url=format!("http://{}",listener.local_addr().unwrap());
    let mock_task=tokio::spawn(async move {axum::serve(listener,mock).await.unwrap()});
    let faucet=FaucetState::new(FaucetConfig {rpc_url:mock_url,drip_amount:1,cooldown_secs:60,..Default::default()},faucet_signer).unwrap();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_url=format!("http://{}/drip",listener.local_addr().unwrap());
    let router=faucet.router();
    let task=tokio::spawn(async move {axum::serve(listener,router).await.unwrap()});
    let address=Address::new([12;20]).to_string();
    let a=client.post(&http_url).json(&json!({"address":address})).send().await.unwrap();
    let b=client.post(&http_url).json(&json!({"address":format!(" {address}")})).send().await.unwrap();
    println!("{}",json!({"case":"http_faucet_whitespace_control","first_status":a.status().as_u16(),"second_status":b.status().as_u16(),"submissions":sends.load(Ordering::SeqCst)}));
    assert_eq!(a.status().as_u16(),200); assert_eq!(b.status().as_u16(),429);
    task.abort();mock_task.abort();
}
