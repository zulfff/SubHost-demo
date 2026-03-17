# Contributing to SubHost 🚀

Halo! Makasih udah mau bantu bikin SubHost jadi lebih keren! 🎉

## Cara Mulai 🏁

### 1. Fork & Clone dulu ya 🔧
```bash
git clone https://github.com/zulfff/SubHost.git
cd SubHost
```

### 2. Setup Environment ⚙️
Pastikan Rust udah keinstall:
```bash
cargo --version  # minimal 1.75 ya
```

Kalau belom, install dulu di [rustup.rs](https://rustup.rs) 🔨

## Workflow 🌊

### 1. Buat Branch Baru 🌿
```bash
git checkout -b fitur-keren-gaes
```

### 2. Coding & Test 🧪
```bash
cargo check   # cek error dulu
cargo test    # jalanin test
cargo fmt     # format code biar rapi
cargo clippy  # cek best practices
```

**Wajib** pass semua test sebelum push! ✅

### 3. Commit & Push 🚀
```bash
git add .
git commit -m "feat: tambah fitur XYZ"
git push origin fitur-keren-gaes
```

### 4. Bikin Pull Request 🎯
Buka GitHub, klik "Compare & Pull Request"
- Deskripsi yang jelas
- Screenshots kalau ada UI changes
- Mention issue kalau related

## Convention Code 📝

### Commit Message Format
```
feat: tambah fitur baru
fix: perbaiki bug consensus
docs: update dokumentasi
refactor: rapihin code
perf: optimasi speed
test: tambah unit test
```

### Rust Style Guide
- Follow `cargo fmt` 📐
- Fix semua `cargo clippy` warning ⚠️
- Document public API pake `///` 📚
- Error handling pake `thiserror` atau `anyhow`

## Area yang Butuh Bantuan 🆘

- 🔐 Crypto & ZK proofs - masih bisa dioptimasi
- 🌐 P2P Networking - libp2p integration
- ⛓️ Consensus - HotStuff/DAG improvements  
- 🧊 EVM compatibility - revm integration
- 📊 Performance benchmarking

## Reporting Bug 🐛

Buka issue dengan format:
- **Judul**: deskriptif & singkat
- **Steps to reproduce**: gimana cara ngebugnya
- **Expected behavior**: harusnya gimana
- **Actual behavior**: malah gimana
- **Environment**: OS, Rust version, etc

## Join Komunitas 💬

Ada pertanyaan? Mau diskusi fitur?
- Buka Discussion tab di GitHub
- Tag maintainers di issue/PR

## Code of Conduct 🤝

- Hormat sesama contributor
- Terima kritik dengan positif
- Fokus ke solusi, bukan masalah
- Bantu yang baru belajar 🌱

---

Dibuat dengan ❤️ oleh komunitas SubHost
Let's build the future of web3 together! 🚀✨
