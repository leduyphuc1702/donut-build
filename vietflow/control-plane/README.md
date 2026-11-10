# Control plane tài khoản tự lưu trữ

Máy chủ nhỏ này thay phần **tài khoản** của Donut cloud cho bản Vietflow. Tài khoản cấu hình trong `.env` là **chủ sở hữu** của một nhóm; chủ sở hữu thêm nhân viên (nhiều người, mỗi người một đăng nhập riêng) ở trang `/admin`. Mọi người dùng chung gói và quyền của chủ sở hữu, không giới hạn số hồ sơ hay số lần tạo, và không phụ thuộc `api.donutbrowser.com`.

Nó **không** chứa dữ liệu hồ sơ. Việc đồng bộ hồ sơ vẫn do [`../sync-server`](../sync-server) (`donut-sync`) đảm nhiệm.

## Nó trả lời những gì

| Nhóm | Đường dẫn | Ghi chú |
| --- | --- | --- |
| Đăng nhập | `/auth/link`, `/api/auth/device-code/*` | `/auth/link`: trang của chủ sở hữu, nhập `CONTROL_TOKEN` để lấy mã dùng một lần (10 phút). Nhân viên dùng mã từ `/admin` (7 ngày) |
| Quản lý thành viên | `/admin`, `/admin/login`, `/admin/members/*` | Trang web cho chủ sở hữu: thêm nhân viên, cấp lại mã, đổi vai trò, đăng xuất các máy, xóa |
| Tài khoản | `/api/auth/me`, `/api/auth/token/refresh` | Tài khoản của đúng người đang đăng nhập: `teamRole` là vai trò của họ, gói và entitlements là của chủ sở hữu |
| Sync token | `/api/auth/sync-token` | JWT `scope=server-sync`, `sub` là người hỏi, gắn với `server` được hỏi |
| Cấu hình đồng bộ | `GET /api/auth/sync-config` | `{ server, token }` lấy từ `SYNC_SERVER_URL` và `SYNC_SHARED_TOKEN`; 404 `SYNC_NOT_PROVIDED` khi thiếu một trong hai. App gọi sau khi đăng nhập để tự bật đồng bộ |
| Khóa hồ sơ | `/api/profile-locks/*` | Mỗi hồ sơ một người giữ khóa tại một thời điểm; khóa hết hạn sau 90 giây nếu máy giữ không gửi heartbeat (30 giây một lần) |
| Nhóm | `GET /api/teams/{id}/members`, `PUT .../members/{user}/access` | Danh sách mọi người và nhóm hồ sơ mỗi người được dùng. Chủ sở hữu và quản trị viên mới xem/sửa được |
| Nhật ký hoạt động | `POST /api/teams/activity`, `GET /api/teams/{id}/activity`, `.../activity.csv` | Nhật ký trong `DATA_DIR/team.json`: ai làm gì, trên máy nào. Chỉ chủ sở hữu và quản trị viên đọc được |
| Agent | `/api/agent/credits`, `/api/agent/token`, `/api/agent/routines`, `.../routines/{id}/runs`, `/api/agent/runs/{id}` | Credits không giới hạn; routine và lần chạy lưu trong `DATA_DIR/agent.json` |
| Vài endpoint phụ | `/api/auth/sync-profile-usage`, `/api/proxy/locations/countries`, `/api/feedback` | Trả lời rỗng/OK để app không lỗi |

Automation cloud, remote MCP, remote control và recipes theo lịch vẫn là dịch vụ của Donut cloud và **không được giả lập**. Ba dịch vụ từng chỉ có trên cloud nay có đường cục bộ và được bật trong entitlements: **cookie bot** chạy trên chính máy đang mở app, **phiên từ xa** mở hồ sơ ngay trên máy đó, và **agent** dùng credits/token/routine của control plane (các tab Chat/Tasks/Routines chỉ hiện khi bản Wayfern đã cài có engine agent). Automation cục bộ (REST/MCP của máy) vẫn bật và không giới hạn.

## Quyền trả về

- `active: true`
- `browserAutomation: true`, `requestsPerHour: 0` (không giới hạn)
- `crossOsFingerprints: true` (mở phần sửa fingerprint; cổng chống lệch OS vẫn chạy như bình thường)
- `profileLimit: 1000000`
- `teamCollaboration: true`, `teamId`/`teamName` lấy từ `TEAM_ID`/`TEAM_NAME`
- `teamRole`: `owner` cho tài khoản trong `.env`; nhân viên là `admin`, `member` hoặc `launcher` theo vai trò chủ sở hữu đặt
- `cookieBot`, `remoteInteractive`, `agent`: `true` (app dùng đường cục bộ; xem mục trên)
- `cloudBackup`, `remoteControl`, `scheduledAutomation`: `false` (chỉ có trên Donut cloud)

Mọi nhân viên nhận cùng gói và entitlements với chủ sở hữu; chỉ `id`, `email`, `teamRole` và `deviceCount` là của riêng họ.

## Nhiều người dùng: thêm nhân viên

Quy trình cho chủ sở hữu:

1. Mở `https://<CONTROL_DOMAIN>/admin`, nhập `CONTROL_TOKEN`. Phiên tự hết hạn sau 30 phút không dùng.
2. Ở mục **Thêm nhân viên**, nhập email và chọn vai trò, bấm **Thêm và tạo mã**.
3. Trang hiện **mã đăng nhập** của người đó. Mã có hiệu lực **7 ngày**, dùng được **một lần** và chỉ hiện đúng lúc này. Gửi mã riêng cho đúng người đó. Cần mã mới (mất tin nhắn, mã hết hạn) thì bấm **Cấp lại mã** ở dòng của họ; mã cũ mất hiệu lực.
4. Nhân viên cài Donut, vào **Tài khoản → Máy chủ**, nhập `https://<CONTROL_DOMAIN>`, lưu. Sau đó bấm **Đăng nhập**, bỏ qua trang web mở ra (trang đó dành cho chủ sở hữu) và **dán mã** vào ô trong Donut. Nếu bản build có `DONUT_SELF_HOST_KEY` thì máy mới mở sẽ tự đăng nhập bằng tài khoản chủ sở hữu; nhân viên cần **Đăng xuất** trước rồi mới đăng nhập bằng mã của mình.
5. Muốn xem nhật ký: chủ sở hữu hoặc quản trị viên vào **Tài khoản → Hoạt động của nhóm** (lọc theo người, loại, thời gian; xuất CSV).

Vai trò:

| Vai trò | Làm được |
| --- | --- |
| `owner` | Tài khoản trong `.env`. Quản lý thành viên ở `/admin`, đọc nhật ký, đặt quyền nhóm cho bất kỳ ai ngoài chính mình |
| `admin` (Quản trị viên) | Đọc nhật ký, xem danh sách thành viên, đặt quyền nhóm cho người không phải `admin`. Không đổi được quyền của `admin` khác hay của chủ sở hữu |
| `member` (Thành viên) | Làm việc với hồ sơ; không đọc nhật ký, không xem danh sách thành viên |
| `launcher` (Người khởi chạy) | Chỉ mở hồ sơ, không sửa cài đặt của nhóm (app tự khóa chỉnh sửa cho vai trò này) |

Các thao tác trên `/admin` (mỗi dòng của một nhân viên):

- **Đổi** vai trò: có hiệu lực ngay ở request kế tiếp, không cần đăng nhập lại.
- **Đăng xuất các máy**: mọi token đang có của người đó hết hiệu lực; họ đăng nhập lại bằng mã mới (**Cấp lại mã**).
- **Xóa** (phải tick **Xác nhận**): người đó mất quyền truy cập ngay, token cũ không dùng được nữa và khóa hồ sơ họ đang giữ được trả lại. Thêm lại cùng email sẽ tạo một người mới.

Ngoài ra, chủ sở hữu và quản trị viên đặt **nhóm hồ sơ** mỗi người được dùng trong app (**Tài khoản → Quyền truy cập của nhóm**). Control plane lưu và trả lại lựa chọn đó (`groupAccess`) và ghi `member.access_change`. **Lưu ý:** việc chặn dữ liệu thật do server đồng bộ thi hành. Khi `donut-sync` dùng `SYNC_TOKEN` chung (cách duy nhất hiện bật được, xem mục Đồng bộ hồ sơ), nó không biết ai đang gọi nên chưa chặn theo nhóm; đừng coi đây là ranh giới bảo mật.

### Nhật ký hoạt động

`TEAM_ID` là một chuỗi cố định (mặc định `local-team`). Mọi bản cài trỏ vào cùng control plane thấy cùng một nhóm và cùng một nhật ký.

- App báo các thay đổi hồ sơ/proxy/nhóm/VPN/extension, **và mỗi lần mở/đóng hồ sơ** (kể cả do REST API hoặc MCP, không ghi địa chỉ trang), vào `POST /api/teams/activity`. Mỗi lô kèm `device` (`id` ngẫu nhiên của bản cài và tên máy). Control plane ghi **người gửi** theo token, không tin người gửi tự khai, và chỉ nhận `device` hợp lệ (`id` tối đa 128 ký tự `A-Za-z0-9_-`, tên tối đa 64 ký tự, bỏ ký tự điều khiển).
- Lần mở/đóng hồ sơ do app báo, không còn do control plane suy ra từ khóa hồ sơ. Vì vậy chỉ máy có team collaboration mới báo, và máy cũ chưa cập nhật chỉ có các sửa đổi, không có mở/đóng.
- Control plane tự ghi các sự kiện thành viên (`member.invite`, `member.join` ở lần đăng nhập đầu, `member.role_change`, `member.remove`, `member.access_change`); app không được báo các sự kiện `member.*`/`invitation.*`.
- Mỗi dòng hiện người làm, tên máy (trong ngoặc, màu nhạt) và việc đã làm. CSV có thêm cột cuối `device`. Ô CSV bắt đầu bằng `=`, `+`, `-`, `@` được thêm dấu `'` phía trước để Excel không chạy thành công thức.
- Nhật ký nằm ở `DATA_DIR/team.json` và giữ 20.000 dòng gần nhất. Xóa file là xóa lịch sử; không có bản sao nào khác.
- Lọc `action` (hỗ trợ `profile.*`), `actorUserId`, `targetType`, `since`; phân trang bằng `cursor`/`limit`; xuất CSV qua `activity.csv`.
- Danh sách thành viên, mã đăng nhập (chỉ lưu dạng băm SHA-256) và máy mỗi người dùng nằm ở `DATA_DIR/members.json`. File hỏng được đổi tên thành `members.json.corrupt-<thời gian>` thay vì bị ghi đè.

### Khóa hồ sơ

Khi một người mở hồ sơ đang đồng bộ, app xin khóa; người thứ hai xin trong lúc khóa còn hiệu lực sẽ bị từ chối và app báo hồ sơ đang do ai dùng. Cùng một người xin lại thì được (mở lại trên máy khác của chính họ). Khóa nằm trong bộ nhớ và tự hết hạn sau 90 giây không có heartbeat, nên máy sập không giữ hồ sơ mãi; khởi động lại control plane cũng xóa khóa, và app xin lại khi trình duyệt vẫn đang chạy.

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
      SYNC_SHARED_TOKEN: ${SYNC_SHARED_TOKEN:-}
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

Trong Donut: **Tài khoản → Máy chủ**, nhập `https://<CONTROL_DOMAIN>`, bấm **Lưu**. Thay đổi này đăng xuất tài khoản cũ (một phiên chỉ thuộc về máy chủ đã phát hành nó).

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

Control plane không thay `donut-sync`. Khi đặt `SYNC_SERVER_URL` và `SYNC_SHARED_TOKEN` (bằng `SYNC_TOKEN` của [`../sync-server`](../sync-server)), mọi máy đăng nhập sẽ tự nhận server và token đồng bộ, không cần nhập gì. App hỏi lại lúc mở và mười phút một lần, nên đổi token trên server thì các máy tự theo. Đăng xuất sẽ xóa token đã nhận khỏi máy. Token này mở toàn bộ kho dữ liệu, nên ai đăng nhập được là đồng bộ được mọi hồ sơ.

Hồ sơ mới mặc định bật đồng bộ (cùng proxy, nhóm, VPN và nhóm tiện ích của nó). Hồ sơ cũ đang tắt đồng bộ cũng được bật một lần khi máy có server đồng bộ. Hồ sơ người dùng tự tắt đồng bộ trên máy nào thì giữ tắt trên máy đó.

Nếu không đặt `SYNC_SHARED_TOKEN`, vẫn có thể nhập tay ở **Tài khoản → Tự lưu trữ** như trước. Sau khi lưu, app dùng server đó và không gửi token tới `sync.donutbrowser.com`.

Cách đơn giản nhất cho một công ty là:

- Control plane: tài khoản + quyền.
- `donut-sync` self-hosted: dữ liệu hồ sơ, mã hóa E2E.

Nếu muốn dùng "Máy chủ đồng bộ của tài khoản" (account sync) thay vì shared token: control plane hiện ký token bằng HS256, còn `donut-sync` xác minh RS256, nên chế độ đó chưa bật. Dùng shared token (`SYNC_TOKEN`) như trên.

## Bảo mật

- `CONTROL_TOKEN` là mật khẩu của chủ sở hữu: lấy mã đăng nhập ở `/auth/link` và vào `/admin`. Không commit `.env`, không gửi qua chat, không đưa cho nhân viên (họ chỉ cần mã của mình).
- `JWT_SECRET` phải là chuỗi ngẫu nhiên riêng; đổi nó sẽ vô hiệu hóa mọi token cũ, kể cả của chủ sở hữu.
- Chạy sau HTTPS. Không mở cổng control plane trực tiếp ra Internet. Cookie của `/admin` là `HttpOnly`, `SameSite=Strict`, chỉ gắn cho đường dẫn `/admin` và thêm `Secure` khi proxy gửi `X-Forwarded-Proto: https` (Caddy làm việc này). Mọi thao tác thay đổi là POST có token CSRF của phiên và bị từ chối nếu `Origin` không phải chính trang này.
- Mỗi token JWT ghi người sở hữu (`sub`) và phiên bản đăng nhập của họ (`tv`). Xóa người hoặc **Đăng xuất các máy** làm token cũ hỏng ngay, không cần chờ hết hạn 30 ngày. Vai trò đọc từ máy chủ ở mỗi request, nên đổi vai trò có hiệu lực ngay. Token cấp trước khi có tính năng thành viên (không có `tv`) vẫn dùng được như của chủ sở hữu.
- Mã đăng nhập dùng một lần: 10 phút nếu lấy ở `/auth/link`, 7 ngày nếu cấp ở `/admin` cho nhân viên. Mã chỉ lưu dạng băm; ai đọc được `members.json` cũng không đăng nhập được bằng nó.
- Server không ghi log token, mã đăng nhập hay nội dung request.
- Ai cầm mã còn hạn là đăng nhập được thành nhân viên đó. Gửi mã qua kênh riêng và cấp lại nếu nghi ngờ.
- Tên máy do chính máy đó khai và chỉ để tham khảo, không phải bằng chứng danh tính: người gửi luôn được xác định bằng token. Tên máy đã bỏ ký tự điều khiển và được escape khi hiện trên `/admin`.
- Nhật ký mở/đóng hồ sơ do chính app của nhân viên báo. Người cố tình sửa app có thể không báo hoặc báo thiếu; nhật ký đáng tin cho vận hành hằng ngày, không phải cho điều tra đối kháng.
- `DATA_DIR/team.json` chứa nhật ký hoạt động (không chứa mật khẩu, token hay nội dung trang) và `DATA_DIR/members.json` chứa danh sách người, vai trò, máy và mã đã băm. Ai đọc được volume này đọc được lịch sử thao tác của cả nhóm.

## Kiểm thử

```sh
cd vietflow/control-plane
npm test
```

Các bài test chạy server thật trên cổng ngẫu nhiên: đăng nhập một lần, dùng lại mã, refresh, sync token, nhiều thành viên (mỗi người một tài khoản, đổi vai trò, xóa, đăng xuất các máy, mã hết hạn, khởi động lại), quyền theo vai trò, trang `/admin` (đăng nhập, cookie, CSRF, escape), nhật ký hoạt động (người gửi, máy, lọc, phân trang, CSV), khóa hồ sơ (xung đột, heartbeat, hết hạn), route lạ và các route cloud-only.
