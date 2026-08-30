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

Project ini memakai satu Cargo workspace dengan 17 crate. Setiap crate punya
tanggung jawab yang sempit:

- `subhost-core` menyimpan tipe inti seperti address, hash, transaksi, dan blok;
- `subhost-crypto` menangani tanda tangan dan komponen dasar kriptografi;
- `subhost-wallet` mengenkripsi dan membuka private key;
- `subhost-mempool` mengatur antrean transaksi yang belum masuk blok;
- `subhost-state` mengatur saldo dan nonce akun;
- `subhost-storage` menulis ledger ke disk secara aman terhadap crash;
- `subhost-rpc` menyediakan endpoint JSON-RPC dan block producer single-node;
- `subhost-node` merangkai genesis, ledger, RPC, dan metrics menjadi satu node;
- `subhost-cli` menjadi pintu masuk perintah dari terminal.

Jumlah crate-nya pernah 23. Tujuh di antaranya ternyata hanya salinan template
yang sama dengan nama tipe berbeda, tidak dipakai crate mana pun, dan tidak
mengerjakan apa pun. Ketujuh crate itu kami hapus. Menyisakannya hanya akan
membuat workspace terlihat lebih besar daripada isinya.

Pemisahan ini bukan sekadar supaya struktur folder terlihat rapi. Kalau aturan
mempool berubah, kami bisa menguji perubahan itu tanpa mengubah seluruh node.

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
cargo build -p subhost-cli
./target/debug/subhost init --chain-id 1 --data-dir ./data \
  --alloc 0x1111111111111111111111111111111111111111=1000000
./target/debug/subhost node --listen 127.0.0.1:8545 --data-dir ./data
```

Lalu dari terminal lain:

```bash
./target/debug/subhost query chain
```

Hasilnya menampilkan `chain_id` dan `height`. Ini membuktikan endpoint hidup dan
memproses request. Ini belum membuktikan ada kesepakatan antar-node, karena
memang hanya ada satu node.

Catatan penting untuk demo: ini adalah single-node producer, bukan konsensus
antar-node. Setelah transaksi lolos validasi, node langsung membuat blok lokal,
mengubah state, membuat receipt, dan menulis ledger ke `node-state.bin`.

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

**State.** State menjalankan transfer dengan memeriksa chain ID, nonce yang tepat,
saldo yang cukup untuk nilai transfer dan gas, lalu menaikkan nonce agar transaksi
yang sama tidak dapat dipakai ulang. Node menyimpan snapshot state dan blok ke disk
secara atomic agar bisa dipulihkan setelah restart.

**Storage.** Ledger ditulis ke file sementara, di-fsync, di-rename secara atomic,
lalu direktorinya juga di-fsync. Saat dibaca ulang, file diperiksa ukuran, magic,
versi, checksum BLAKE3, dan chain ID-nya, kemudian seluruh commitment blok dan
receipt dihitung ulang terhadap state yang dipulihkan. File yang rusak ditolak,
bukan dimuat setengah-setengah.

**JSON-RPC.** Tersedia sepuluh method: `eth_chainId`, `net_version`,
`eth_blockNumber`, `eth_gasPrice`, `eth_getBalance`, `eth_getTransactionCount`,
`eth_sendTransaction`, `eth_getTransactionReceipt`, `eth_getBlockByNumber`, dan
`eth_getTransactionByHash`. `eth_sendTransaction` memeriksa signature Ed25519 dan
mengikat public key ke alamat pengirim, lalu producer single-node mengeksekusi dan
menulis ledger sebelum state di memori diubah.

**Metrics.** `subhost node --metrics-addr` menyalakan exporter Prometheus dengan
`/metrics` dan `/health`, dan melaporkan tinggi blok serta kedalaman mempool.

## Slide 6 - Bagian yang Belum Boleh Disebut Selesai

Di sini batasannya penting.

Perintah `node` saat ini menyalakan JSON-RPC dengan single-node block producer.
Transaksi valid diproses menjadi blok lokal dan bisa dicari receipt-nya. Namun,
CLI belum menyalakan jaringan P2P dan belum menjalankan loop konsensus antar-node.

Modul konsensus sudah punya struktur DAG, HotStuff, quorum, dan staking, tetapi
belum menjadi loop konsensus produksi yang berjalan antar-node.

EVM, WASM, zero-knowledge, dan governance tidak ada di repository ini. Sebelumnya
ada crate dengan nama itu, tetapi isinya template kosong, jadi kami hapus dan
sekarang README menyebutkan terus terang bahwa keempatnya belum dikerjakan.
Perintah `contract deploy` dan `contract call` juga kami hapus dari CLI: keduanya
hanya mencetak satu baris log lalu melaporkan sukses.

`subhost-network` sudah bisa mengirim dan menerima pesan gossip, tetapi belum ada
node yang memakainya untuk menyebarkan blok. `subhost-ibc` menjalankan state
machine paket secara lokal, tetapi belum memverifikasi proof dari chain lain.

Semua endpoint jaringan juga belum punya autentikasi maupun TLS, jadi harus
dijalankan di loopback atau di belakang proxy.

Jadi kesimpulan teknisnya: SubHost sekarang adalah **blockchain single-node yang
benar-benar berjalan**, bukan jaringan terdistribusi dan bukan cloud
terdesentralisasi yang siap dipakai.

## Slide 7 - Keamanan yang Sudah Diperhatikan

Untuk keamanan, kami memulai dari validasi yang paling dekat dengan data dan
transaksi.

- Private key wallet dienkripsi dengan scrypt dan AES-256-GCM, bukan teks biasa.
- File wallet ditulis atomic, dibatasi ukurannya, dan permission `0600` di-set
  sebelum key ditulis, bukan sesudahnya.
- Saat dibuka, alamat di file harus cocok dengan private key hasil dekripsi.
- RPC menolak signature yang tidak cocok dengan public key atau alamat pengirim.
- RPC menolak chain ID yang salah dan nonce yang tidak sesuai state.
- Semua aritmetika saldo dan nonce memakai `checked_*`; overflow jadi error.
- Mempool membatasi ukuran data, gas, kapasitas antrean, dan prioritas.
- Registrasi validator wajib menyertakan proof of possession, supaya agregasi BLS
  tidak bisa diserang dengan rogue key.
- Quorum certificate hanya diterima kalau cukup banyak validator berbeda yang
  sudah terdaftar masing-masing memberi signature yang valid. Sebelumnya fungsi
  ini selalu mengembalikan `false`, jadi tidak pernah bisa dipakai.
- Slashing sekarang benar-benar memotong stake. Sebelumnya jumlah potongan
  dihitung lalu dikembalikan tanpa pernah dikurangkan.
- Faucet sekarang menandatangani dan mengirim transfer sungguhan. Sebelumnya ia
  mengembalikan hash palsu hasil hash alamat dan waktu, tanpa transaksi apa pun.

Kami juga menulis test untuk aturan-aturan tersebut. Tetapi ini perlu disebut
jelas: test internal bukan pengganti audit keamanan pihak ketiga. Threat model
di repository adalah daftar risiko dan rencana mitigasi; belum berarti semua
mitigasinya sudah aktif di jaringan produksi.

## Slide 8 - Cara Kami Menguji Project

Pengujian kami dibagi menjadi beberapa tingkat.

Pertama, `cargo fmt` dan `cargo clippy` untuk seluruh workspace dengan
`-D warnings`. Satu peringatan saja membuat build gagal.
Kedua, unit test di setiap crate: 202 test yang menguji jalur gagal, bukan hanya
jalur sukses. Ada test untuk file ledger yang di-bit-flip, replay nonce, overflow
saldo, signature dari kunci lain, dan input yang tidak valid.
Ketiga, smoke test end-to-end: jalankan node, buat wallet, kirim transfer, cek
receipt, restart node, dan pastikan saldo tetap.
Keempat, `cargo deny` untuk memeriksa advisory keamanan, lisensi, dan sumber
dependency.

Semua itu dijalankan otomatis di CI sebagai tujuh job wajib, termasuk pemeriksaan
versi Rust minimum dan release build.

Karena mesin pengembangan hanya sekitar 4 GB RAM, `.cargo/config.toml` membatasi
build ke dua job. Lebih lambat, tetapi tidak kena OOM.

## Slide 9 - Rencana Pengembangan

Prioritas berikutnya kami susun berdasarkan urutan dependensi, bukan berdasarkan
jumlah fitur di slide.

Dua hal pertama di rencana sebelumnya sudah selesai: CLI kini membentuk signature
dan mengambil nonce sendiri dari node, dan ledger sudah punya checksum, penulisan
atomic, serta validasi penuh saat dibaca ulang.

Yang berikutnya:

1. Menjalankan loop konsensus dan menyambungkan gossip antar-node, memakai
   primitif quorum yang sudah ada dan sudah diuji.
2. Membuat satu blok bisa memuat lebih dari satu transaksi.
3. Mengganti state root dengan struktur Merkle supaya bisa membuat proof.
4. Memverifikasi proof light client untuk IBC.
5. Setelah alur dasar stabil, baru eksekusi smart contract.

Urutan ini penting. Smart contract tidak banyak artinya kalau blok belum bisa
disepakati antar-node.

## Slide 10 - Penutup

SubHost adalah blockchain single-node yang berjalan, ditulis dengan Rust. Yang
sudah ada dan bisa diuji: tipe data inti, kriptografi, wallet terenkripsi,
mempool, aturan state, block producer, ledger yang tahan crash, JSON-RPC dengan
signature wajib, exporter metrics, CLI, faucet, dan explorer.

Yang belum: konsensus antar-node, penyebaran blok, dan eksekusi smart contract.

Selama pengerjaan ini kami juga menghapus 51 file yang tidak pernah ter-compile
dan memperbaiki empat fungsi yang terlihat bekerja tetapi sebenarnya tidak: quorum
certificate yang selalu ditolak, quorum DAG yang mengukur hal yang salah, slashing
yang tidak memotong apa pun, dan faucet yang mengembalikan hash palsu.

Kami memilih menyampaikan batasan ini secara jelas supaya hasil project dinilai
dari kode dan pengujian yang benar-benar ada.

Terima kasih atas perhatian Bapak/Ibu dan teman-teman. Kami siap menerima
pertanyaan dan masukan.

Wassalamualaikum warahmatullahi wabarakatuh.

## Catatan Jawaban Saat Tanya Jawab

**"Apakah ini sudah blockchain yang berjalan penuh?"**

Sebagai satu node, ya: transaksi ditandatangani, diverifikasi, dieksekusi, masuk
blok, dan ledger-nya bertahan setelah restart. Sebagai jaringan, belum: belum ada
konsensus antar-node dan belum ada penyebaran blok.

**"Kapan receipt masih `null`?"**

Untuk hash yang belum ditemukan, receipt memang `null`. Untuk transaksi valid yang
sudah diproses oleh single-node producer, receipt berisi hash blok, tinggi blok,
gas, status, dan daftar log.

**"Apakah Docker-nya bisa dipakai?"**

Bisa. `docker compose up --build` menyalakan satu node, explorer, dan Prometheus.
Semua port hanya dibuka di loopback dan container berjalan sebagai user biasa
dengan filesystem read-only. Tetap satu node, bukan testnet multi-node, karena
block producer-nya memang single-node. Untuk demo di kelas, binary CLI lebih
ringan di mesin 4 GB.

**"Apa bukti fitur ini sudah ada?"**

Jawab dengan tiga jenis bukti: source code pada crate terkait, unit test, dan smoke
test yang bisa diulang. Untuk fitur yang baru berupa kerangka, kami sebut sebagai
kerangka.

**"Apa kontribusi utama project ini?"**

Kontribusinya adalah membangun batas dan kontrak awal antar-komponen node: format
transaksi, validasi signature, aturan mempool, perubahan state, dan endpoint RPC.
Itu menjadi dasar sebelum jaringan dan konsensus produksi ditambahkan.
