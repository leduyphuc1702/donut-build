# Control plane tài khoản tự lưu trữ

Máy chủ nhỏ này thay phần **tài khoản** của Donut cloud cho bản Vietflow. Nó trả về một tài khoản duy nhất với đầy đủ quyền cần dùng, không giới hạn số hồ sơ hay số lần tạo, và không phụ thuộc `api.donutbrowser.com`.

Nó **không** chứa dữ liệu hồ sơ. Việc đồng bộ hồ sơ vẫn do [`../sync-server`](../sync-server) (`donut-sync`) đảm nhiệm.

## Nó trả lời những gì

| Nhóm | Đường dẫn | Ghi chú |
| --- | --- | --- |
| Đăng nhập | `/auth/link`, `/api/auth/device-code/*` | Trang nhập `CONTROL_TOKEN`, sinh mã dùng một lần |
| Tài khoản | `/api/auth/me`, `/api/auth/token/refresh` | User + entitlements |
| Sync token | `/api/auth/sync-token` | JWT `scope=server-sync`, gắn với `server` được hỏi |
| Khóa hồ sơ | `/api/profile-locks/*` | Một chủ sở hữu; không chặn hai máy cùng mở một hồ sơ — dùng lease của `donut-sync` nếu cần |
| Nhóm | `/api/teams/{id}/members`, `PUT .../members/{user}/access` | Nhóm một ghế: tài khoản là chủ sở hữu. Không giới hạn được ghế chủ |
| Nhật ký hoạt động | `POST /api/teams/activity`, `GET /api/teams/{id}/activity`, `.../activity.csv` | Nhật ký cục bộ trong `DATA_DIR/team.json`, lọc và phân trang như cloud |
| Agent | `/api/agent/credits`, `/api/agent/token`, `/api/agent/routines`, `.../routines/{id}/runs`, `/api/agent/runs/{id}` | Credits không giới hạn; routine và lần chạy lưu trong `DATA_DIR/agent.json` |
| Vài endpoint phụ | `/api/auth/sync-profile-usage`, `/api/proxy/locations/countries`, `/api/feedback` | Trả lời rỗng/OK để app không lỗi |

Automation cloud, remote MCP, remote control và recipes theo lịch vẫn là dịch vụ của Donut cloud và **không được giả lập**. Ba dịch vụ từng chỉ có trên cloud nay có đường cục bộ và được bật trong entitlements: **cookie bot** chạy trên chính máy đang mở app, **phiên từ xa** mở hồ sơ ngay trên máy đó, và **agent** dùng credits/token/routine của control plane (các tab Chat/Tasks/Routines chỉ hiện khi bản Wayfern đã cài có engine agent). Automation cục bộ (REST/MCP của máy) vẫn bật và không giới hạn.

## Quyền trả về

- `active: true`
- `browserAutomation: true`, `requestsPerHour: 0` (không giới hạn)
- `crossOsFingerprints: true` (mở phần sửa fingerprint; cổng chống lệch OS vẫn chạy như bình thường)
- `profileLimit: 1000000`
- `teamCollaboration: true`, `teamId`/`teamName` lấy từ `TEAM_ID`/`TEAM_NAME`, `teamRole: "owner"`
- `cookieBot`, `remoteInteractive`, `agent`: `true` (app dùng đường cục bộ; xem mục trên)
- `cloudBackup`, `remoteControl`, `scheduledAutomation`: `false` (chỉ có trên Donut cloud)

### Nhóm và nhật ký hoạt động cục bộ

`TEAM_ID` là một chuỗi cố định (mặc định `local-team`). Mọi bản cài trỏ vào cùng control plane thấy cùng một nhóm và cùng một nhật ký; chưa có tài khoản thứ hai nên danh sách thành viên chỉ có ghế chủ.

- App báo các thay đổi hồ sơ/proxy/nhóm/VPN/extension vào `POST /api/teams/activity`; control plane ghi thêm lần mở/đóng hồ sơ khi nhận/trả profile lock. Nhật ký nằm ở `DATA_DIR/team.json` (Docker: volume `control_data`).
- Nhật ký giữ 20.000 dòng gần nhất. Xóa file là xóa lịch sử; không có bản sao nào khác.
- Lọc `action` (hỗ trợ `profile.*`), `actorUserId`, `targetType`, `since`; phân trang bằng `cursor`/`limit`; xuất CSV qua `activity.csv`.
- Vì chỉ có một tài khoản, "phân quyền theo nhóm" không có người thứ hai để áp dụng. Cần nhiều người dùng thật thì phải bổ sung xác thực đa tài khoản trước.

## Chạy trên VPS

Yêu cầu: Ubuntu 24.04, Docker, một tên miền trỏ về VPS.

```sh
cd donutbrowser/vietflow/control-plane
cp .env.example .env
openssl rand -hex 32   # CONTROL_TOKEN
openssl rand -hex 32   # JWT_SECRET
nano .env              # điền CONTROL_DOMAIN, CONTROL_TOKEN, JWT_SECRET
docker compose up -d --build
curl https://<CONTROL_DOMAIN>/healthz
```

Caddy tự xin chứng chỉ HTTPS. Chỉ Caddy mở 80/443; control plane chỉ nghe trong mạng compose.

### Nếu VPS đã chạy Caddy của `sync-server`

Không chạy thêm Caddy thứ hai: nó sẽ tranh cổng 80/443. Thêm service control plane vào **cùng** file compose của `sync-server` (cùng network với Caddy):

```yaml
  control-plane:
    build: ../control-plane
    restart: unless-stopped
    environment:
      PORT: "8787"
      CONTROL_TOKEN: ${CONTROL_TOKEN:?CONTROL_TOKEN is required}
      JWT_SECRET: ${JWT_SECRET:?JWT_SECRET is required}
      ACCOUNT_ID: ${ACCOUNT_ID:-selfhosted-owner}
      ACCOUNT_EMAIL: ${ACCOUNT_EMAIL:-owner@selfhosted.local}
      ACCOUNT_PLAN: ${ACCOUNT_PLAN:-enterprise}
      SYNC_SERVER_URL: ${SYNC_SERVER_URL:-}
      TEAM_ID: ${TEAM_ID:-local-team}
      TEAM_NAME: ${TEAM_NAME:-Vietflow}
      DATA_DIR: /data
    volumes:
      - control_data:/data
    read_only: true
    tmpfs:
      - /tmp
    security_opt:
      - no-new-privileges:true
    cap_drop:
      - ALL
```

Thêm `control_data:` vào khối `volumes:` của file compose dùng chung (file này đã khai báo sẵn nếu chạy `docker compose` riêng trong thư mục control plane).

Thêm vào Caddyfile hiện có (Caddy gọi service theo tên, không qua cổng host):

```caddyfile
control.example.com {
  reverse_proxy control-plane:8787
}
```

Điền `CONTROL_TOKEN`, `JWT_SECRET`, `CONTROL_DOMAIN` vào `.env` của `sync-server`, rồi:

```sh
cd vietflow/sync-server
docker compose up -d --build control-plane caddy
```

### Chạy thử trong LAN, không HTTPS

Bỏ service `caddy`, mở cổng `8787` trong `docker-compose.yml`, rồi đặt địa chỉ `http://<IP nội bộ>:8787` trong app. Chỉ làm việc này trong mạng tin cậy: `CONTROL_TOKEN` đi qua kết nối không mã hóa.

### Chạy trực tiếp bằng Node

```sh
cd vietflow/control-plane
CONTROL_TOKEN="$(openssl rand -hex 32)" JWT_SECRET="$(openssl rand -hex 32)" npm start
npm test
```

## Nối app vào control plane

Trong Donut: **Tài khoản → Server**, nhập `https://<CONTROL_DOMAIN>`, bấm **Lưu**. Thay đổi này đăng xuất tài khoản cũ (một phiên chỉ thuộc về máy chủ đã phát hành nó).

Sau đó bấm **Đăng nhập**:

1. App mở `https://<CONTROL_DOMAIN>/auth/link`.
2. Nhập `CONTROL_TOKEN`, bấm **Lấy mã đăng nhập**.
3. Dán mã một lần vào cái ô trong Donut.

Nếu muốn đặt sẵn cho cả bản build, dùng biến môi trường `DONUT_CLOUD_API_URL=https://<CONTROL_DOMAIN>`; app vẫn ưu tiên giá trị đã lưu trong **Tài khoản → Server**.

### Lần mở đầu tiên đã là pro

Bản build phát cho cả công ty có thể tự lấy tài khoản chung, không cần mã:

```sh
DONUT_SELF_HOST_URL="https://<CONTROL_DOMAIN>" \
DONUT_SELF_HOST_KEY="<AUTO_LOGIN_KEY>" \
pnpm tauri build
```

- `AUTO_LOGIN_KEY` lấy từ `.env` của control plane (để trống thì dùng `CONTROL_TOKEN`).
- Lần mở đầu tiên, nếu máy chưa có phiên, app gọi `POST /api/auth/auto` và nhận đúng tài khoản pro của control plane. Các lần sau dùng token đã lưu, không gọi lại.
- `DONUT_SELF_HOST_URL` / `DONUT_SELF_HOST_KEY` đặt lúc build được nhúng vào binary. Khi chạy dev có thể đặt cùng biến trong môi trường để trỏ một binary vào control plane nội bộ mà không build lại.
- Giá trị đã lưu trong **Tài khoản → Server** vẫn thắng giá trị nhúng.

**Bảo mật:** khóa này nằm trong mọi bản build phát ra, nên ai có file cài đều lấy được tài khoản chung. Chỉ phát trong nội bộ. Muốn thu hồi, đổi `AUTO_LOGIN_KEY` trên server rồi phát bản build mới; token cũ vẫn sống tới khi hết hạn (30 ngày) nên đổi `JWT_SECRET` nếu cần cắt ngay.

## Đồng bộ hồ sơ

Control plane không thay `donut-sync`. Cấu hình **Tài khoản → Tự lưu trữ** (self-hosted sync) với `SYNC_TOKEN` của [`../sync-server`](../sync-server). Với tài khoản tự host, mục này vẫn mở sau khi đăng nhập (tài khoản tự host không có cloud backup), nên bản build tự đăng nhập vẫn đặt được server đồng bộ. Sau khi lưu, app dùng server đó và không gửi token tới `sync.donutbrowser.com`.

Cách đơn giản nhất cho một công ty là:

- Control plane: tài khoản + quyền.
- `donut-sync` self-hosted: dữ liệu hồ sơ, mã hóa E2E.

Nếu muốn dùng "Máy chủ đồng bộ của tài khoản" (account sync) thay vì shared token: control plane hiện ký token bằng HS256, còn `donut-sync` xác minh RS256, nên chế độ đó chưa bật. Dùng shared token (`SYNC_TOKEN`) như trên.

## Bảo mật

- `CONTROL_TOKEN` là mật khẩu lấy mã đăng nhập. Không commit `.env`, không gửi qua chat.
- `JWT_SECRET` phải là chuỗi ngẫu nhiên riêng; đổi nó sẽ vô hiệu hóa mọi token cũ.
- Chạy sau HTTPS. Không mở cổng control plane trực tiếp ra Internet.
- Mã đăng nhập dùng một lần, hết hạn sau 10 phút.
- Server không ghi log token, mã đăng nhập hay nội dung request.
- Một tài khoản duy nhất. "Nhóm" chỉ có nghĩa một ghế chủ; muốn nhiều người thật phải bổ sung xác thực và phân quyền trước khi mở ra ngoài.
- `DATA_DIR/team.json` chứa nhật ký hoạt động (không chứa mật khẩu, token hay nội dung trang). Ai đọc được volume này đọc được lịch sử thao tác của tài khoản.

## Kiểm thử

```sh
cd vietflow/control-plane
npm test
```

Các bài test chạy server thật trên cổng ngẫu nhiên: đăng nhập một lần, dùng lại mã, refresh, sync token, profile lock, nhóm một ghế, nhật ký hoạt động (lọc, phân trang, CSV, lần mở/đóng hồ sơ), route lạ và các route cloud-only.
