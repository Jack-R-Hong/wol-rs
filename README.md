# wol-rs

零依賴(Rust std only)的 Wake-on-LAN 工具:

- **CLI 模式**:在 RPi3 上一行直接喚醒同 LAN 的 PC
- **HTTP 模式**:提供 `GET/POST /wake?mac=...` API,搭配 Cloudflare Tunnel 實現異地喚醒

## 原理

RPi3 常開 → Cloudflare Tunnel 把它的 HTTP 服務暴露到 `https://wol.example.com`
→ 手機/別台機器呼叫 API → RPi3 以「有線網卡」的子網 broadcast(如 x.x.255.255:9)送出 magic packet
→ 交換器中繼 broadcast → PC 網卡收到、BIOS WoL 啟用 → 開機。

**雙網卡(wlan + lan)注意事項**:broadcast 會依 routing 選發射卡,容易走錯卡。
本程式會掃描 `/sys/class/net`,**預設優先有線卡**,並 bind 該卡 IP 向該子網 broadcast 發送;
之後仍會順帶發給其他介面與 255.255.255.255 作保底。
若機器有填 IP,則該 IP 所在子網的介面自動置頂。

## 使用

### CLI(LAN 內直接喚醒)

```sh
wolrs aa:bb:cc:dd:ee:ff            # 預設:有線卡優先
wolrs aa:bb:cc:dd:ee:ff eth0       # 指定發射網卡
WOLRS_IFACE=enp6s0 wolrs aa:bb:cc:dd:ee:ff   # 或用環境變數全域指定
```

### HTTP server

```sh
PORT=8787 ./wolrs        # 預設即 8787
curl "http://127.0.0.1:8787/wake?mac=aa:bb:cc:dd:ee:ff"
# 指定發射網卡:
curl "http://127.0.0.1:8787/api/wake?name=pc1&iface=enp6s0"
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
