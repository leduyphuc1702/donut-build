# Bản Donut của Vietflow

Bản fork này dựa trên Donut Browser (AGPL-3.0, giữ nguyên tên và giấy phép). So với bản gốc, nó có thêm:

- **"Đóng ở máy kia và mở ở đây".** Một profile đồng bộ chỉ chạy trên một máy tại một thời điểm.
  - Máy muốn dùng profile gửi yêu cầu. Máy đang giữ đóng trình duyệt sao cho cookie kịp ghi xuống đĩa, đẩy bản mới nhất lên rồi nhả khóa. Sau đó máy yêu cầu mới mở.
  - Máy bị chiếm cưỡng chế cũng tự đóng trình duyệt, để hai máy không bao giờ chạy cùng một phiên.
  - Phần client nằm ở `src-tauri/src/sync/lease.rs`, phần máy chủ ở `donut-sync/src/locks.rs`.
- **"Dán bằng cách gõ"**: phím tắt Ctrl+Alt+V (⌘⌥V trên Mac).
  - Gõ nội dung đã copy vào ô đang chọn, từng phím như người, thay vì dán. Kết quả cuối luôn đúng từng ký tự.
  - Nhấn Esc hoặc bấm chuột để dừng. Bật/tắt trong Cài đặt → Quyền riêng tư.
  - Mã nguồn: `src-tauri/src/paste_typing.rs`.
- **Cập nhật tự động từ repo này** (`leduyphuc1702/donutbrowser`), không lấy bản gốc (`src-tauri/src/app_auto_updater.rs`).

Máy chủ đồng bộ: xem repo `d2-us-tunnel`, thư mục `sync-server/` (mini PC) hoặc `sync-server/vps/` (VPS Ubuntu).

## Cài đặt lần đầu

Tải file cài ở [Releases](https://github.com/leduyphuc1702/donutbrowser/releases):

- **macOS:**
  - Chọn `Donut_<phiên bản>_aarch64.dmg` cho Mac M1/M2/M3/M4, hoặc `Donut_<phiên bản>_x64.dmg` cho Mac Intel.
  - Bản build chưa có chứng chỉ Apple. Sau khi kéo vào Applications, chạy một lần trong Terminal:
    ```sh
    xattr -dr com.apple.quarantine /Applications/Donut.app
    ```
  - Lần đầu dùng "Dán bằng cách gõ", macOS hỏi quyền Trợ năng: bật Donut trong Cài đặt hệ thống → Quyền riêng tư & Bảo mật → Trợ năng.
  - Vì bản build chưa có chứng chỉ Apple, sau mỗi lần cập nhật có thể phải tắt rồi bật lại quyền này.
- **Windows:**
  - Chạy `Donut_<phiên bản>_x64-setup.exe`. Windows SmartScreen sẽ cảnh báo vì chưa ký số: bấm "More info" → "Run anyway".
  - Nếu không muốn cài, có thể dùng bản portable `Donut_<phiên bản>_x64-portable.zip`.

Từ lần sau, Donut tự kiểm tra bản mới trên repo này mỗi giờ.

## Phát hành bản mới

1. Đưa thay đổi lên nhánh dùng để build.
2. Tạo tag phiên bản **lớn hơn** bản trước, rồi push tag:
   ```sh
   git tag v0.36.2 && git push origin v0.36.2
   ```
   Hoặc vào tab **Actions → Vietflow release → Run workflow** và nhập tag.
3. Workflow `.github/workflows/vietflow-release.yml` build bản macOS (Apple Silicon và Intel) và Windows, tải lên Releases, rồi tạo `SHA256SUMS.txt`. Trình cập nhật chỉ cài bản kiểm chứng được bằng file này.

Lưu ý:
- Lần đầu cần bật GitHub Actions cho repo fork: tab **Actions** → "I understand my workflows, go ahead and enable them".
- Số phiên bản do tag quyết định; không cần sửa tay `tauri.conf.json`.
