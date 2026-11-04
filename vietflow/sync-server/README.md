# Máy chủ đồng bộ cho Donut bản Vietflow

Máy chủ này giữ bản sao các hồ sơ dùng chung, và làm trọng tài để mỗi hồ sơ chỉ chạy trên một máy tại một thời điểm. Nó là `donut-sync` của repo này, có thêm lệnh "đóng ở máy kia, mở ở đây". Bản `donutbrowser/donut-sync` chính thức không có lệnh đó.

Có hai cách chạy:
- **VPS Ubuntu, có tên miền và HTTPS** (thư mục này). Dùng được từ văn phòng lẫn ở nhà.
- **Một máy trong mạng nội bộ, không HTTPS** (`lan/`). Chỉ dùng trong mạng mà mọi thiết bị đều tin cậy.

Dữ liệu được mã hóa hai lớp:
- Bật **Đồng bộ mã hóa E2E** trên từng hồ sơ. Dữ liệu được mã hóa ngay trên máy nhân viên bằng mật khẩu chỉ công ty biết, nên máy chủ chỉ thấy dữ liệu đã mã hóa.
- Máy chủ mã hóa thêm một lớp bằng `SYNC_AT_REST_KEY`.

## 1. VPS Ubuntu

Đề xuất: Ubuntu 24.04, 1–2 vCPU, 2 GB RAM, ổ 40 GB trở lên (mỗi hồ sơ khoảng 50–300 MB). Chọn VPS gần nơi nhân viên làm việc (Việt Nam hoặc Singapore) để đồng bộ nhanh.

```sh
curl -fsSL https://get.docker.com | sh
ufw allow OpenSSH && ufw allow 80/tcp && ufw allow 443/tcp && ufw --force enable
git clone -b claude/optimize-browser-us-purchasing-aherea https://github.com/leduyphuc1702/donutbrowser.git
cd donutbrowser/vietflow/sync-server
cp .env.example .env
openssl rand -hex 32                     # SYNC_TOKEN
openssl rand -hex 20                     # S3_ACCESS_KEY_ID
openssl rand -hex 20                     # S3_SECRET_ACCESS_KEY
echo "k1:$(openssl rand -base64 32)"     # SYNC_AT_REST_KEY
nano .env                                # điền các giá trị trên và SYNC_DOMAIN
docker compose up -d --build
curl https://<SYNC_DOMAIN>/health        # {"status":"ok"}
```

**Tên miền:**
- Có tên miền: tạo bản ghi A, ví dụ `sync.vietflow.com`, trỏ về IP của VPS.
- Chưa có: dùng `<IP với dấu gạch ngang>.sslip.io`, ví dụ IP `203.0.113.7` thành `203-0-113-7.sslip.io`.

Caddy tự xin chứng chỉ HTTPS ở lần chạy đầu.

## 2. Hoặc một máy trong mạng nội bộ

```sh
cd donutbrowser/vietflow/sync-server/lan
cp .env.example .env    # điền như trên, bỏ qua SYNC_DOMAIN
docker compose up -d --build
```

Địa chỉ máy chủ là `http://<IP nội bộ của máy đó>:12342`. Nên giữ IP này cố định bằng cách đặt IP tĩnh, hoặc giữ chỗ DHCP trên router.

## 3. Trên từng máy Donut

1. **Tài khoản → Tự lưu trữ**: nhập URL máy chủ và token (`SYNC_TOKEN`), rồi bấm "Kiểm tra kết nối".
2. Đặt **mật khẩu mã hóa đầu cuối**, giống nhau trên mọi máy.
3. Với mỗi hồ sơ dùng chung, chọn **Đồng bộ mã hóa E2E**.
4. Mọi máy chạy Donut bản Vietflow. Máy chạy bản gốc sẽ bị coi là "không phản hồi" khi người khác yêu cầu dùng hồ sơ.

Lưu `SYNC_TOKEN` và `SYNC_AT_REST_KEY` ở nơi an toàn. Mất `SYNC_AT_REST_KEY` là mất toàn bộ dữ liệu đã đồng bộ.

## 4. Quy tắc dùng chung

- Muốn dùng hồ sơ đang mở ở máy khác: bấm **"Đóng ở máy kia và mở ở đây"**, rồi chờ đồng bộ xong.
- **"Mở bản đồng bộ gần nhất"** chỉ dùng khi máy kia tắt hoặc mất mạng. Trang web có thể bắt đăng nhập lại.
- Hồ sơ tạo trên Windows chỉ mở trên Windows, tạo trên Mac chỉ mở trên Mac.

## 5. Sao lưu và cập nhật

```sh
cd donutbrowser/vietflow/sync-server        # hoặc .../lan
docker compose stop && tar czf ~/donut-sync-$(date +%F).tgz data .env && docker compose start
git pull && docker compose up -d --build    # cập nhật
```

Bản sao lưu chứa khóa trong `.env`, nên phải giữ kín.

Xem log: `docker compose logs -f donut-sync`. Các dòng `Lease ...` cho biết máy nào đang giữ, đang yêu cầu, hoặc vừa nhả hồ sơ.
