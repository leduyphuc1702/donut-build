# Một VPS: control plane + sync server

`vietflow/control-plane` (tài khoản, entitlements, khóa hồ sơ) và
`vietflow/sync-server` (`donut-sync`, đồng bộ hồ sơ) chạy chung một máy sau
một Caddy. Mỗi thứ một tên miền con, Caddy tự xin HTTPS.

## Build image ở máy khác rồi đưa lên

VPS nhỏ (1 vCPU, 1 GB RAM) không nên tự build `donut-sync` (Rust release):
build trên máy có Docker rồi chuyển image.

```sh
# trên máy build
docker buildx build --platform linux/amd64 -t donut-sync:latest donut-sync --load
docker buildx build --platform linux/amd64 -t donut-control-plane:latest vietflow/control-plane --load
docker save donut-sync:latest donut-control-plane:latest | gzip > donut-images.tgz

# trên VPS
scp donut-images.tgz root@<ip>:/tmp/
ssh root@<ip> 'gunzip -c /tmp/donut-images.tgz | docker load'
```

## Chạy

```sh
scp -r vietflow/vps root@<ip>:/opt/donut
ssh root@<ip>
cd /opt/donut
cp .env.example .env
chmod 600 .env
# điền tên miền và các khóa: openssl rand -hex 32
docker compose up -d
curl https://<CONTROL_DOMAIN>/healthz
curl https://<SYNC_DOMAIN>/healthz
```

Dữ liệu nằm trong `data/control` (nhật ký nhóm, thành viên) và `data/s3`
(hồ sơ đã đồng bộ, mã hóa bằng `SYNC_AT_REST_KEY`). Chứng chỉ HTTPS nằm trong
volume `caddy_data`.

Đừng dùng `sslip.io` cho bản phát cho cả công ty nếu có thể mua tên miền: đổi
tên miền sau đó phải sửa địa chỉ server trong app trên từng máy.
