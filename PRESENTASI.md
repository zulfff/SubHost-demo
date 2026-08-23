# Naskah Presentasi - SubHost Web3

> Naskah ini dibuat untuk dibaca saat presentasi. Teks dalam kurung siku adalah
> arahan untuk presenter dan tidak perlu dibaca keras-keras.

## Slide 1 - Pembukaan

Assalamualaikum warahmatullahi wabarakatuh.

Yang kami hormati Bapak/Ibu dosen pembimbing dan teman-teman semua.

Kami dari kelompok 1. Anggota kelompok kami adalah [sebutkan nama anggota].

Hari ini kami akan mempresentasikan **SubHost Web3**, yaitu project Rust yang
sedang kami bangun sebagai pondasi untuk node blockchain dengan API, mempool,
state, kriptografi, dan modul konsensus yang dipisah dengan jelas.

Kami akan menjelaskan dua hal secara seimbang: bagian yang sudah bisa diuji dan
bagian yang masih menjadi pekerjaan berikutnya. Jadi, kami tidak akan menyebut
fitur sebagai "sudah jadi" hanya karena crate atau kerangkanya sudah ada.

## Slide 2 - Masalah yang Ingin Diselesaikan

Ide awalnya adalah membuat infrastruktur yang tidak bergantung pada satu server
saja. Dalam sistem seperti ini, banyak node menyimpan dan memproses data bersama.
Blockchain dipakai untuk menjaga urutan transaksi dan kesepakatan antar-node,
sedangkan penyimpanan dan komputasi bisa dikembangkan di lapisan berikutnya.

Namun, membuat blockchain bukan hanya membuat satu fungsi untuk menambah blok.
Ada beberapa bagian yang harus bekerja bersama:

- transaksi harus punya format dan identitas yang jelas;
- tanda tangan harus bisa diverifikasi;
- transaksi yang masuk perlu diantrekan sebelum diproses;
- saldo dan nonce harus diperbarui tanpa menerima transaksi lama dua kali;
- client harus punya cara untuk berkomunikasi dengan node.

SubHost kami mulai dari bagian-bagian dasar itu. Target besarnya adalah
infrastruktur cloud terdesentralisasi, tetapi target project saat ini adalah
membangun fondasi yang dapat diuji satu per satu.

## Slide 3 - Kenapa Memakai Rust dan Banyak Crate

Bahasa utama yang kami gunakan adalah **Rust**. Alasannya praktis: Rust punya
kontrol memori yang kuat, performanya baik, dan banyak kesalahan bisa ditangkap
saat compile sebelum program dijalankan.

Project ini memakai satu Cargo workspace dengan 23 crate. Setiap crate punya
tanggung jawab yang lebih sempit, misalnya:

- `subhost-core` menyimpan tipe inti seperti address, hash, transaksi, dan blok;
- `subhost-crypto` menangani tanda tangan dan komponen dasar kriptografi;
- `subhost-mempool` mengatur antrean transaksi yang belum masuk blok;
- `subhost-state` mengatur saldo dan nonce akun;
- `subhost-rpc` menyediakan endpoint JSON-RPC;
- `subhost-cli` menjadi pintu masuk perintah dari terminal.

Pemisahan ini bukan sekadar supaya struktur folder terlihat rapi. Kalau aturan
mempool berubah, kami bisa menguji perubahan itu tanpa harus mengubah seluruh
node. Batas antarbagian juga lebih mudah diperiksa saat code review.

## Slide 4 - Alur Sistem yang Saat Ini Bisa Didemokan

Alur yang benar-benar sudah tersambung sekarang adalah sebagai berikut:

1. CLI menjalankan server JSON-RPC pada alamat yang dipilih.
2. RPC membaca state akun dan menerima transaksi yang sudah ditandatangani.
3. RPC memeriksa chain ID, alamat, signature, dan nonce.
4. Transaksi yang lolos dimasukkan ke mempool.
5. State menyediakan aturan untuk menjalankan transfer, membayar gas, dan menaikkan nonce.

[Tunjukkan diagram alur atau terminal.]

Contoh menjalankan demo:

```bash
cargo build -p subhost-cli -j 1
./target/debug/subhost init --chain-id 1 --data-dir ./data
./target/debug/subhost node --listen 127.0.0.1:8545
```

Lalu dari terminal lain:

```bash
curl -s http://127.0.0.1:8545 \
  -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}'
```

Respons yang diharapkan adalah `"0x1"`. Ini membuktikan endpoint hidup dan
memproses request. Ini belum membuktikan bahwa jaringan blockchain sudah
memproduksi blok.

Catatan penting untuk demo: aturan `apply_transaction` memang ada di crate state,
tetapi saat ini belum dipanggil oleh block producer. Alur node yang aktif berhenti
di mempool; eksekusi state masih diuji sebagai komponen terpisah.

## Slide 5 - Implementasi yang Sudah Ada

Bagian yang sudah kami implementasikan dan uji meliputi:

**Tipe data inti.** Ada tipe untuk address, hash, header blok, blok, transaksi,
receipt, dan konfigurasi genesis. Tipe ini menjadi kontrak data antar-crate.

**Kriptografi.** Ada BLS12-381 untuk tanda tangan dan agregasi, proof of
possession untuk mengikat kunci dengan pemiliknya, serta X25519 dan enkripsi
simetris untuk kebutuhan pertukaran atau penyimpanan kunci. Wallet memakai
scrypt dan AES-GCM untuk mengenkripsi private key saat disimpan.

**Mempool.** Mempool menolak gas limit nol, gas price terlalu rendah, dan data
transaksi yang terlalu besar. Ia juga menangani nonce per pengirim, penggantian
transaksi dengan nonce sama jika gas price lebih tinggi, deduplikasi, batas per
pengirim, dan eviksi transaksi dengan prioritas paling rendah ketika penuh.

**State.** State saat ini berada di memori. Untuk transfer, state memeriksa chain
ID, nonce yang tepat, saldo yang cukup untuk nilai transfer dan gas, lalu menaikkan
nonce agar transaksi yang sama tidak dapat dipakai ulang.

**JSON-RPC.** Subset method yang tersedia antara lain `eth_chainId`,
`eth_blockNumber`, `eth_getBalance`, `eth_sendTransaction`, `eth_gasPrice`, dan
`net_version`. `eth_sendTransaction` memeriksa signature Ed25519 dan memasukkan
transaksi valid ke mempool.

## Slide 6 - Bagian yang Belum Boleh Disebut Selesai

Di sini batasannya penting.

Perintah `node` saat ini menyalakan JSON-RPC dengan state dan mempool in-memory.
CLI belum menyalakan jaringan P2P dan belum menjalankan loop produksi blok.
Artinya transaksi bisa diterima sebagai transaksi pending, tetapi belum ada
proses yang mengonfirmasi transaksi itu menjadi blok. Karena itu,
`eth_getTransactionReceipt` mengembalikan `null`.

Modul konsensus sudah punya struktur DAG, HotStuff, quorum, dan staking, tetapi
belum menjadi loop konsensus produksi yang berjalan antar-node.

Crate EVM, WASM, dan zero-knowledge masih berupa kerangka atau modul terpisah.
Perintah `contract deploy` dan `contract call` di CLI belum mengeksekusi bytecode.
Modul network, storage, metrics, faucet, IBC, dan governance juga masih parsial
dan belum bisa dianggap sebagai layanan production-ready.

Jadi kesimpulan teknisnya: SubHost sekarang adalah **fondasi node dan API yang
memiliki beberapa komponen nyata**, bukan testnet publik dan bukan cloud
terdesentralisasi yang sudah siap dipakai.

## Slide 7 - Keamanan yang Sudah Diperhatikan

Untuk keamanan, kami memulai dari validasi yang paling dekat dengan data dan
transaksi.

- Private key wallet dienkripsi saat disimpan, bukan ditulis sebagai teks biasa.
- RPC menolak signature yang tidak cocok dengan public key atau alamat pengirim.
- RPC menolak chain ID yang salah dan nonce yang tidak sesuai state.
- Mempool memberi batas pada ukuran data, gas, kapasitas antrean, dan prioritas.
- BLS menyediakan proof of possession agar agregasi signature tidak menerima
  kunci yang tidak terbukti dimiliki.

Kami juga menulis test untuk aturan-aturan tersebut. Tetapi ini perlu disebut
jelas: test internal bukan pengganti audit keamanan pihak ketiga. Threat model
di repository adalah daftar risiko dan rencana mitigasi; belum berarti semua
mitigasinya sudah aktif di jaringan produksi.

## Slide 8 - Cara Kami Menguji Project

Pengujian kami dibagi menjadi beberapa tingkat.

Pertama, compile check untuk memastikan seluruh workspace lolos pemeriksaan
compiler.
Kedua, unit test pada crate seperti CLI, mempool, state, dan crypto.
Ketiga, smoke test: jalankan node, kirim request JSON-RPC, dan cek responsnya.
Keempat, kami cek kondisi repository dengan build package yang memang punya
binary, bukan menganggap semua contoh di dokumentasi pasti sudah aktif.

Karena mesin pengembangan hanya memiliki sekitar 4 GB RAM, build dijalankan
dengan satu job Cargo, misalnya `cargo build ... -j 1`. Ini memang lebih lambat,
tetapi mengurangi risiko proses compiler berebut memori dan terkena OOM.

## Slide 9 - Rencana Pengembangan

Prioritas berikutnya kami susun berdasarkan urutan dependensi, bukan berdasarkan
jumlah fitur di slide.

1. Menyambungkan transaksi dari wallet atau client sampai ke node dengan signature
   dan nonce yang benar.
2. Menambahkan penyimpanan state yang persisten, sehingga data tidak hilang saat
   proses berhenti.
3. Menghubungkan mempool ke producer dan eksekutor blok.
4. Menjalankan loop konsensus dan komunikasi antar-node secara nyata.
5. Setelah alur dasar stabil, baru mengembangkan eksekusi EVM/WASM, metrics,
   governance, dan fitur lintas-chain.

Urutan ini penting. Smart contract dan benchmark tidak banyak artinya kalau
transaksi belum bisa masuk blok dan state belum bisa dipulihkan setelah restart.

## Slide 10 - Penutup

SubHost adalah project yang sedang membangun fondasi blockchain node dengan Rust.
Saat ini kami sudah memiliki tipe data inti, kriptografi, mempool, state in-memory,
dan JSON-RPC yang bisa dijalankan serta diuji.

Di sisi lain, produksi blok, P2P, konsensus antar-node, persistence, dan eksekusi
smart contract belum selesai. Kami memilih menyampaikan batasan ini secara jelas
supaya hasil project dapat dinilai dari kode dan pengujian yang benar-benar ada.

Terima kasih atas perhatian Bapak/Ibu dan teman-teman. Kami siap menerima
pertanyaan dan masukan.

Wassalamualaikum warahmatullahi wabarakatuh.

## Catatan Jawaban Saat Tanya Jawab

**"Apakah ini sudah blockchain yang berjalan penuh?"**

Belum. Yang sudah berjalan adalah node JSON-RPC dengan mempool dan state in-memory.
Loop P2P, produksi blok, dan finalisasi konsensus belum tersambung ke CLI.

**"Kenapa receipt masih `null`?"**

Karena belum ada block producer dan confirmation pipeline. Transaksi bisa masuk
ke mempool, tetapi belum ada proses yang memasukkannya ke blok dan membuat receipt.

**"Kenapa tidak memakai Docker untuk demo?"**

Docker di repository masih berupa referensi arsitektur dan belum menjadi testnet
yang bisa langsung dijalankan. Untuk demo, binary CLI dengan `-j 1` lebih jelas dan
lebih ringan untuk mesin RAM 4 GB.

**"Apa bukti fitur ini sudah ada?"**

Jawab dengan tiga jenis bukti: source code pada crate terkait, unit test, dan smoke
test yang bisa diulang. Untuk fitur yang baru berupa kerangka, kami sebut sebagai
kerangka.

**"Apa kontribusi utama project ini?"**

Kontribusinya adalah membangun batas dan kontrak awal antar-komponen node: format
transaksi, validasi signature, aturan mempool, perubahan state, dan endpoint RPC.
Itu menjadi dasar sebelum jaringan dan konsensus produksi ditambahkan.
