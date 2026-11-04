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

Máy chủ đồng bộ: xem [`vietflow/sync-server/`](../vietflow/sync-server/README.md). Có hai cách: VPS Ubuntu, hoặc một máy trong mạng nội bộ.

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

## Độ kín: những gì Donut tự lo, những gì phải làm tay

Không công cụ nào cam kết kín 100% trước mọi thuật toán: độ trễ mạng, cách nhiều tài khoản dùng chung một IP, hay nhịp thao tác đều đo được. Mục tiêu là không để lộ khác biệt nào kiểm soát được.

**Donut bản Vietflow tự chặn:**
- **Mở hồ sơ khi lối ra không khớp vân tay.**
  - Hồ sơ gắn WireGuard hoặc proxy vốn đã được đo lối ra. Hồ sơ không có proxy/VPN nào (đi thẳng bằng mạng của máy) giờ cũng được đo. Ví dụ: một hồ sơ Mỹ lỡ bị gỡ WireGuard sẽ không mở bằng IP Việt Nam.
  - Donut đo cả IPv4 lẫn IPv6 trước khi mở, rồi hỏi lại thay vì mở luôn. Tắt kiểm tra này ở Cài đặt → Quyền riêng tư → "Chặn khi dấu vân tay không khớp" (không nên tắt).
- **Hai máy chạy cùng một phiên**: xem phần "Đóng ở máy kia".
- **Mỗi máy tự sinh một thiết bị khác nhau cho cùng hồ sơ**: thiết bị sinh lúc mở giờ được đồng bộ sang các máy khác.
- **"Tạo ngẫu nhiên khi khởi chạy" trên hồ sơ đồng bộ**: tùy chọn này bị bỏ qua, vì nó biến mỗi lần mở thành một thiết bị lạ.

**Phải làm theo quy tắc:**
1. **Gắn WireGuard (VPN) vào hồ sơ ngay khi tạo, trước lần mở đầu tiên.** Donut lấy múi giờ, ngôn ngữ và tọa độ của vân tay theo lối ra của hồ sơ lúc tạo, nên lối ra Mỹ phải có trước.
2. **Bật Cài đặt → Quyền riêng tư → "Chặn khởi chạy khi không có proxy hoặc VPN".** Không hồ sơ nào mở được bằng mạng của máy, kể cả khi WireGuard của nó hỏng hoặc bị xóa. Khi hồ sơ đi qua WireGuard của Donut, DNS, WebRTC và mọi kết nối của trình duyệt đều đi trong tunnel, nên IPv6 hay DNS của mạng máy không lộ ra.
3. **Cùng một phiên bản Donut và Wayfern trên mọi máy.**
   - Phiên bản trình duyệt nằm trong User-Agent, và nhiều trang so nó với lúc đăng nhập.
   - Phiên bản tụt lùi (máy B cũ hơn máy A) là bất thường. Cập nhật tất cả các máy cùng lúc.
4. **Không bật "Danh sách chặn DNS" (chặn quảng cáo) trên hồ sơ dùng để mua.** Nó chặn luôn script chống gian lận của Amazon, eBay và Shopify; trang không đo được thiết bị sẽ chấm rủi ro cao hơn.
5. **Không cài tiện ích (extension) vào hồ sơ mua hàng**, trừ khi thật cần. Tiện ích bị trang đo được.
6. **Không phóng to cửa sổ lớn hơn màn hình mà vân tay khai báo.**
   - Hồ sơ dùng chung nên chạy trên các máy có màn hình cỡ gần nhau.
   - Log của Donut cảnh báo `Claimed screen larger than the display` khi màn hình khai báo lớn hơn màn hình thật.
7. **Gỡ AnyDesk/TeamViewer** khỏi máy dùng để mua. Một số trang từng dò cổng của chúng.
8. **Gõ thay vì dán** (Ctrl+Alt+V / ⌘⌥V). Với Shopify, đừng đi thẳng link → giỏ hàng → thanh toán trong vài chục giây: xem thêm 1–2 trang trước.
9. **Hạn chế số tài khoản trên cùng một IP.** Mọi hồ sơ dùng chung một WireGuard thì ra cùng một IP. Vài tài khoản trên một IP nhà dân là bình thường; vài chục tài khoản cùng một IP, cùng thẻ, cùng kho nhận hàng thì bị liên kết với nhau. Khi số tài khoản tăng, cần thêm lối ra Mỹ thứ hai, thứ ba.

**Còn hở, không phần mềm nào vá được:**
- **Độ trễ khoảng 200 ms** từ Việt Nam sang Mỹ. Trang có thể đo được, và nó lệch với một IP nhà dân ở Mỹ. Chỉ trình duyệt chạy ngay trên máy ở Mỹ mới hết.
- **Dấu TCP của WireGuard trong Donut.** WireGuard của Donut tự dựng gói TCP bằng một bộ TCP chạy trong ứng dụng (smoltcp), không qua Windows hay macOS. Bản Vietflow đã cho gói tin mang TTL đúng của hệ điều hành máy (128 trên Windows, 64 trên Mac). Kích thước cửa sổ và thứ tự tùy chọn TCP vẫn khác của Windows hay macOS thật. Chỉ công cụ dò hệ điều hành theo gói tin (p0f) mới đọc được chi tiết này.
- **MSS 1340** (gói TCP nhỏ hơn bình thường, do tunnel). Đây là dấu hiệu "đang dùng VPN" của cả văn phòng.
- **Kết quả vẽ đồ họa** (canvas, WebGL) vẫn do GPU và driver thật tạo ra rồi được Wayfern làm nhiễu. Hai máy khác GPU có thể ra kết quả khác nhau.
- **Thông số máy giả lập khác máy thật:** số nhân CPU và RAM khai báo có thể lệch với tốc độ chạy thật đo bằng JavaScript.
- **Cổng gỡ lỗi (CDP) của trình duyệt** mở trên 127.0.0.1. Chromium mới đã chặn trang công khai dò cổng localhost, nhưng đây vẫn là bề mặt cần biết.

**Tự kiểm tra (nên làm với mỗi máy mới, và khi thấy bị bắt đăng nhập lại):**
1. Mở cùng một hồ sơ lần lượt trên 2 máy, mỗi lần chạy các trang dưới đây và **so kết quả giữa 2 máy**. Mọi mục phải giống nhau, trừ những gì đã nêu ở phần "còn hở":
   - <https://browserleaks.com/ip>, <https://browserleaks.com/webrtc>, <https://browserleaks.com/dns>: IP, WebRTC và DNS đều ở Mỹ, không có IPv6.
   - <https://browserleaks.com/canvas>, <https://browserleaks.com/webgl>, <https://browserleaks.com/fonts>: hash canvas, WebGL và danh sách font.
   - <https://pixelscan.net>: báo "consistent" (nhất quán), không có "masking detected".
   - <https://abrahamjuliot.github.io/creepjs/>: mục "lies" (dối trá) càng ít càng tốt; ghi lại để so giữa 2 máy.
   - <https://demo.fingerprint.com/playground>: "Bot: not detected", và mã visitor ID **giống nhau** trên 2 máy.
2. Nếu hash canvas hoặc WebGL khác nhau giữa 2 máy: hai máy đó không nên dùng chung hồ sơ. Ghép cặp theo phần cứng giống nhau.
