# wol-rs

零依賴(Rust std only)的 Wake-on-LAN 工具:

- **CLI 模式**:在 RPi3 上一行直接喚醒同 LAN 的 PC
- **HTTP 模式**:提供 `GET/POST /wake?mac=...` API,搭配 Cloudflare Tunnel 實現異地喚醒

## 原理

RPi3 常開 → Cloudflare Tunnel 把它的 HTTP 服務暴露到 `https://wol.example.com`
→ 手機/別台機器呼叫 API → RPi3 往 255.255.255.255:9 播送 magic packet
→ 交換器中繼 broadcast → PC 網卡收到、BIOS WoL 啟用 → 開機。

## 使用

### CLI(LAN 內直接喚醒)

```sh
wolrs aa:bb:cc:dd:ee:ff
```

### HTTP server

```sh
PORT=8787 ./wolrs        # 預設即 8787
curl "http://127.0.0.1:8787/wake?mac=aa:bb:cc:dd:ee:ff"
# => 200 OK  "woken: aa:bb:cc:dd:ee:ff"
```

### 在 RPi3 上建置

```sh
cargo build --release
# 跨平台(從 x86 編給 Pi):
# rustup target add armv7-unknown-linux-gnueabihf
# cargo build --release --target armv7-unknown-linux-gnueabihf
sudo install -m755 target/release/wolrs /usr/local/bin/wolrs
sudo systemctl enable --now wol   # 用本 repo 的 wol.service
```

## Cloudflare Tunnel

### 快速驗證(quick tunnel)

```sh
cloudflared tunnel --url http://127.0.0.1:8787
# 出現 https://xxxx.trycloudflare.com 後:
curl "https://xxxx.trycloudflare.com/wake?mac=aa:bb:cc:dd:ee:ff"
```

### 正式(named tunnel)

參照 `cloudflared.example.yml`,或:

```sh
cloudflared tunnel login
cloudflared tunnel create wol
cloudflared tunnel route dns <tunnel-id> wol.example.com
# 將 config 放到 ~/.cloudflared/config.yml
cloudflared tunnel --config ~/.cloudflared/config.yml run wol
```

## PC 端設定(必要)

1. BIOS/UEFI 啟用 **Wake on LAN**(ErP 設 off 或 1.0/1.2)
2. 網卡進階設定:WoL **Enabled**,勾選「僅回應 magic packet」
3. **有線網卡**最可靠;Wi-Fi 較挑硬體
4. 確認 MAC 可被喚醒:
   - Windows: `getmac /v` 看 `Wake-on-Lan capable: Yes`
   - Linux: `ethtool <dev>` 看 `Wake-on: g`
5. PC 需「關機/休眠」狀態(留待機電),直接斷電則無效

## 注意安全

magic packet 本身不驗證,遠端 API 建議加一層:

- **Cloudflare Access**(免費)套在 hostname 上做 OIDC 驗證(推薦)
- 或在程式端加 `?token=...` 比對
