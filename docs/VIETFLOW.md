# Bản Donut của Vietflow

Bản fork này dựa trên Donut Browser và chuyển sang giấy phép MIT. So với bản gốc, nó có thêm:

- **"Đóng ở máy kia và mở ở đây".** Một profile đồng bộ chỉ chạy trên một máy tại một thời điểm.
  - Máy muốn dùng profile gửi yêu cầu. Máy đang giữ đóng trình duyệt sao cho cookie kịp ghi xuống đĩa, đẩy bản mới nhất lên rồi nhả khóa. Sau đó máy yêu cầu mới mở.
  - Máy bị chiếm cưỡng chế cũng tự đóng trình duyệt, để hai máy không bao giờ chạy cùng một phiên.
  - Phần client nằm ở `src-tauri/src/sync/lease.rs`, phần máy chủ ở `donut-sync/src/locks.rs`.
- **"Dán bằng cách gõ"**: trong trình duyệt do Donut mở, cứ nhấn **Ctrl+V (⌘V trên Mac) như bình thường**.
  - Nội dung ngắn một dòng (số thẻ, tên, địa chỉ, email, tối đa 120 ký tự) được gõ vào ô đang chọn, từng phím như người, thay vì dán. Kết quả cuối luôn đúng từng ký tự.
  - Link, văn bản dài, nhiều dòng, ảnh: vẫn dán bình thường. Ứng dụng khác ngoài trình duyệt của Donut không bị ảnh hưởng.
  - Ctrl+Alt+V (⌘⌥V) gõ trong mọi ứng dụng, kể cả văn bản dài tới 500 ký tự.
  - Không áp dụng cho: **Dán** trong menu chuột phải và **tự điền (autofill)** của trình duyệt. Hai cách này không đi qua bàn phím; muốn trang thấy thao tác gõ thì dùng Ctrl+V/⌘V, và đừng lưu thẻ/địa chỉ vào autofill của hồ sơ mua hàng.
  - Nhấn Esc hoặc bấm chuột để dừng. Bật/tắt trong Cài đặt → Quyền riêng tư.
  - Mã nguồn: `src-tauri/src/paste_typing.rs`.
- **Cập nhật tự động từ repo này** (`leduyphuc1702/donutbrowser`), không lấy bản gốc (`src-tauri/src/app_auto_updater.rs`).
- **Khóa hồ sơ vào một thị trường.** Trong hộp thoại hồ sơ, phần proxy/VPN, chọn **Thị trường** (ví dụ "Hoa Kỳ"). Từ đó hồ sơ chỉ mở được khi lối ra đang ở nước đó.
  - Trước mỗi lần mở, Donut đo lối ra thật: qua proxy/VPN của hồ sơ, hoặc qua mạng của chính máy (cả IPv4 lẫn IPv6) khi hồ sơ không gắn proxy/VPN.
  - Lối ra ở nước khác, hoặc không đo được: hồ sơ không mở, không có nút "mở luôn". Tự động hóa cũng bị chặn như người.
  - Dùng cho máy phải nối WiFi Mỹ: chọn Thị trường = Hoa Kỳ và không gắn proxy/VPN. Chưa nối đúng WiFi thì không mở được.
  - Mã nguồn: `launch_gate.rs::enforce_exit_country`, `fingerprint_consistency.rs::measure_exits`.

Máy chủ đồng bộ: xem [`vietflow/sync-server/`](../vietflow/sync-server/README.md). Có hai cách: VPS Ubuntu, hoặc một máy trong mạng nội bộ.

## Cài đặt lần đầu

Tải file cài ở [Releases](https://github.com/leduyphuc1702/donutbrowser/releases):

- **macOS:**
  - Chọn `Donut_<phiên bản>_aarch64.dmg` cho Mac M1/M2/M3/M4, hoặc `Donut_<phiên bản>_x64.dmg` cho Mac Intel.
  - Bản build chưa có chứng chỉ Apple. Sau khi kéo vào Applications, chạy một lần trong Terminal:
    ```sh
    xattr -dr com.apple.quarantine /Applications/Donut.app
    ```
  - Lần đầu một trình duyệt của Donut được đưa lên trước, macOS hỏi quyền Trợ năng cho "Dán bằng cách gõ": bật Donut trong Cài đặt hệ thống → Quyền riêng tư & Bảo mật → Trợ năng. Chưa cấp quyền thì Ctrl+V/⌘V vẫn dán bình thường.
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
2. **Khóa mọi hồ sơ mua hàng vào thị trường của nó** (trường "Thị trường"). Hồ sơ gắn WireGuard mà WireGuard hỏng, hay máy chưa nối WiFi Mỹ, đều không mở được. Với hồ sơ chỉ đi qua WireGuard của Donut, bật thêm **Cài đặt → Quyền riêng tư → "Chặn khởi chạy khi không có proxy hoặc VPN"**. Khi hồ sơ đi qua WireGuard của Donut, DNS, WebRTC và mọi kết nối của trình duyệt đều đi trong tunnel, nên IPv6 hay DNS của mạng máy không lộ ra.
   - Máy nối WiFi Mỹ (hồ sơ không gắn proxy/VPN): tắt IPv6 trên máy, hoặc chắc chắn WiFi đó không cấp IPv6. Donut đo cả IPv6 và chặn khi nó ở nước khác, nhưng tắt từ gốc vẫn hơn.
3. **Cùng một phiên bản Donut trên mọi máy.**
   - Engine Apostate được ghim theo từng bản Donut, nên cùng bản Donut là cùng phiên bản Chromium trong User-Agent; nhiều trang so nó với lúc đăng nhập.
   - Phiên bản tụt lùi (máy B cũ hơn máy A) là bất thường. Cập nhật tất cả các máy cùng lúc.
4. **Không bật "Danh sách chặn DNS" (chặn quảng cáo) trên hồ sơ dùng để mua.** Nó chặn luôn script chống gian lận của Amazon, eBay và Shopify; trang không đo được thiết bị sẽ chấm rủi ro cao hơn.
5. **Không cài tiện ích (extension) vào hồ sơ mua hàng**, trừ khi thật cần. Tiện ích bị trang đo được.
6. **Không phóng to cửa sổ lớn hơn màn hình mà vân tay khai báo.**
   - Hồ sơ dùng chung nên chạy trên các máy có màn hình cỡ gần nhau.
   - Log của Donut cảnh báo `Claimed screen larger than the display` khi màn hình khai báo lớn hơn màn hình thật.
7. **Gỡ AnyDesk/TeamViewer** khỏi máy dùng để mua. Một số trang từng dò cổng của chúng.
8. **Gõ thay vì dán**: dùng ⌘V/Ctrl+V, không dùng "Dán" ở menu chuột phải hay autofill. Với Shopify, đừng đi thẳng link → giỏ hàng → thanh toán trong vài chục giây: xem thêm 1–2 trang trước.
9. **Hạn chế số tài khoản trên cùng một IP.** Mọi hồ sơ dùng chung một WireGuard thì ra cùng một IP. Vài tài khoản trên một IP nhà dân là bình thường; vài chục tài khoản cùng một IP, cùng thẻ, cùng kho nhận hàng thì bị liên kết với nhau. Khi số tài khoản tăng, cần thêm lối ra Mỹ thứ hai, thứ ba.

**Còn hở, không phần mềm nào vá được:**
- **Độ trễ khoảng 200 ms** từ Việt Nam sang Mỹ. Trang có thể đo được, và nó lệch với một IP nhà dân ở Mỹ. Chỉ trình duyệt chạy ngay trên máy ở Mỹ mới hết.
- **Dấu TCP của WireGuard trong Donut.** WireGuard của Donut tự dựng gói TCP bằng một bộ TCP chạy trong ứng dụng (smoltcp), không qua Windows hay macOS. Bản Vietflow đã cho gói tin mang TTL đúng của hệ điều hành máy (128 trên Windows, 64 trên Mac). Kích thước cửa sổ và thứ tự tùy chọn TCP vẫn khác của Windows hay macOS thật. Chỉ công cụ dò hệ điều hành theo gói tin (p0f) mới đọc được chi tiết này.
- **MSS 1340** (gói TCP nhỏ hơn bình thường, do tunnel). Đây là dấu hiệu "đang dùng VPN" của cả văn phòng.
- **Kết quả vẽ đồ họa** (canvas, WebGL) vẫn do GPU và driver thật tạo ra rồi được Apostate làm nhiễu. Hai máy khác GPU có thể ra kết quả khác nhau.
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

## Đo mạng thực tế (2026-11-05)

Máy đo: macOS, Wi-Fi văn phòng đi qua WireGuard US.

- IP: `71.236.224.43`, Comcast AS7922, Portland/Hillsboro, Oregon; loại residential; không có IPv6; WebRTC chỉ thấy IP exit.
- DNS: NextDNS US (`45.90.28.0`/`45.90.30.0`), egress Vultr/Misaka tại Mỹ. Khác ISP Comcast nhưng cùng country.
- Latency: ~200–216 ms tới 1.1.1.1, Amazon, eBay, Shopify. Đây là residual của đường Việt Nam → US → site.
- MTU tunnel: 1380; MSS bị clamp còn 1340; TTL giảm 2 hop.
- p0f/browserleaks: SYN từ exit có MSS 1340, options của Darwin (nop, wscale 6, timestamps, sackOK, eol), bị xếp **iOS/Darwin**, link type **OpenVPN UDP**. Không phải Windows/Linux. Đây là lý do profile Windows không thể chạy trên máy macOS này.

Hồ sơ khai OS khác máy chạy app (ví dụ hồ sơ Windows trên Mac) vẫn mở được, kể cả khi không gắn proxy/VPN: Donut chỉ cảnh báo lúc tạo hồ sơ (`fingerprint.crossOsApostateWarning`) rằng tầng TCP/mạng vẫn lộ OS của máy khi không đi qua WireGuard. Nếu hồ sơ đã gắn proxy/VPN mà lối đó đang hỏng thì launch gate trả `FINGERPRINT_HOST_OS_MISMATCH` và không mở, vì đó là lối hỏng chứ không phải lựa chọn. Hồ sơ mua hàng nên chạy trên máy cùng OS với hồ sơ; muốn p0f khớp hẳn với Windows thì chạy Donut trong Windows VM/máy Windows thật, dùng bridged/TAP hoặc kernel NAT; không dùng user-mode NAT (QEMU slirp) vì nó terminate TCP và lộ stack host. Chạy app trong VM, không chỉ browser.

## Tài khoản tự host (control plane)

Từ bản này, tài khoản không cần Donut cloud. `vietflow/control-plane/` là một server nhỏ trả về một tài khoản duy nhất với:

- không giới hạn số hồ sơ mỗi giờ và số trình duyệt chạy cùng lúc;
- `requestsPerHour: 0`, nên automation cục bộ (REST/MCP trên máy) không bị đếm;
- bật quyền sửa vân tay; hồ sơ mới sinh thiết bị ngay trên máy, không cần token hay cloud;
- hồ sơ khai OS khác máy vẫn mở được (kèm cảnh báo về tầng TCP/mạng); chỉ chặn khi proxy/VPN đã gắn mà lối đó hỏng;
- bật nhóm cục bộ một ghế: tài khoản là chủ sở hữu, nhật ký hoạt động (mở/đóng hồ sơ, sửa hồ sơ/proxy/nhóm/VPN/extension) lưu tại control plane và xem/xuất CSV được trong **Tài khoản → Nhật ký hoạt động**;
- bật cookie bot, phiên từ xa và dịch vụ agent cục bộ (chi tiết bên dưới). Remote MCP (kết nối từ donutbrowser.com) vẫn là dịch vụ cloud; automation cục bộ qua REST/MCP vẫn chạy.

### Dịch vụ cục bộ thay cho dịch vụ cloud

- **Cookie Bot** chạy trên chính máy này (`src-tauri/src/cookie_bot_local.rs`). Lịch, lần chạy, thống kê và mẫu lưu trong `settings/cookie_bot_local.json`. Mỗi lần một hồ sơ, chỉ khi Donut đang mở, chỉ với hồ sơ cùng OS với máy và không ephemeral; không cần proxy/exit node và không tiêu "giờ từ xa" (mức hiển thị chỉ là danh nghĩa). Lệch vân tay với lối ra chỉ ghi log, không chặn lần chạy nền.
- **Phiên từ xa** mở hồ sơ ngay trên máy này. Nút **Chạy trên máy này** trong chi tiết hồ sơ (và mục tương ứng trong menu chuột phải) tạo một phiên có trạng thái đang chạy, dừng được, và dùng chung đường launch/CDP với phiên cloud. Máy chủ phiên chính là máy đang chạy Donut.
- **Agent**: control plane trả credits, token, routine và lần chạy tại `/api/agent/*`. Agent tasks điều khiển trình duyệt qua miền CDP `Wayfern.*` của bản Wayfern đóng, mà Apostate không có, nên các tab Chat/Tasks/Routines và Recipes/Schedules bị ẩn và nhiệm vụ agent bị từ chối bằng `APOSTATE_FEATURE_UNSUPPORTED`. Muốn điều khiển hồ sơ bằng agent thì dùng MCP cục bộ trong **Tích hợp**.

Giới hạn của các dịch vụ cục bộ:

- Cookie Bot chỉ chạy khi app mở; đóng app là lịch dừng. Mỗi lúc chỉ một lần chạy.
- Danh sách phiên từ xa nằm trong bộ nhớ: khởi động lại app thì phiên không còn trong danh sách; nếu trình duyệt vẫn mở, dừng nó bằng nút Stop thường.
- Team chỉ có một ghế owner; không có lời mời thành viên hay phân quyền nhóm.
- Agent tasks, ghi recipe và synchronizer cần miền CDP `Wayfern.*` nên không chạy được trên Apostate; chỉ dùng MCP cục bộ.

Cách dùng:

1. Dựng server theo `vietflow/control-plane/README.md`.
2. Trong Donut: **Tài khoản → Server**, nhập `https://control...`, bấm Lưu.
3. Bấm **Đăng nhập**, mở trang `/auth/link`, nhập `CONTROL_TOKEN`, lấy mã dùng một lần rồi dán vào Donut.

Bản build phát cho cả công ty không cần bước 3: build với `DONUT_SELF_HOST_URL` và `DONUT_SELF_HOST_KEY` (khóa tự đăng nhập của control plane), lần mở đầu tiên app tự nhận tài khoản pro. Khóa nằm trong binary nên chỉ phát nội bộ; đổi khóa trên server rồi phát bản mới khi cần thu hồi. Chi tiết trong `vietflow/control-plane/README.md`.

Đồng bộ hồ sơ vẫn do `vietflow/sync-server/` đảm nhiệm; control plane không giữ dữ liệu hồ sơ. Khi tài khoản là tự host, app không gửi token của tài khoản đó tới `sync.donutbrowser.com`; nó dùng server đồng bộ tự host đã cấu hình. Mục **Tài khoản → Tự lưu trữ** vẫn mở sau khi đăng nhập tài khoản tự host, nên bản build tự đăng nhập vẫn đặt được `SYNC_TOKEN`.

## Engine: chỉ còn Apostate

Donut bản Vietflow không còn dùng Wayfern. Mọi hồ sơ chạy trên **Apostate** (`github.com/heretic-tech/apostate`, bản 0.5.1, Chromium 155.0.8059.31, GPL-3.0; dùng nội bộ nên không vướng nghĩa vụ phát hành). Apostate là file chạy tải riêng, Donut không liên kết vào mã của mình. Chuỗi `wayfern` vẫn còn trong ID trình duyệt, tên khóa lưu trữ và dữ liệu đồng bộ để dữ liệu cũ đọc được; người dùng không còn thấy tên Wayfern trên giao diện.

**Lần mở đầu tiên.**
- Không có màn tải trình duyệt, không có điều khoản Wayfern, không có token. Mở app là dùng được ngay.
- Donut tự tải Apostate ở nền ngay khi khởi động (~160 MB, sha256 được ghim trong `src-tauri/src/apostate.rs`). Có tiến trình nhỏ ở góc màn hình; mất mạng thì tự thử lại, giãn dần tới 5 phút một lần. Bản tải dở do tắt app giữa chừng, bản Apostate cũ và thư mục Wayfern cũ trong `binaries/` được xóa ở lần khởi động sau.
- Trong lúc đó vẫn tạo được hồ sơ (hộp thoại tạo hồ sơ có dòng "đang chuẩn bị engine"). Bấm mở hồ sơ trước khi engine tải xong thì Donut chờ bản tải đang chạy và hiện bước "Đang chuẩn bị engine" thay vì báo lỗi.
- Sai checksum thì xóa bản tải và báo `APOSTATE_CHECKSUM_MISMATCH`; lỗi mạng báo `APOSTATE_DOWNLOAD_FAILED`; máy không có bản dựng (ví dụ Windows ARM) báo `APOSTATE_UNSUPPORTED_HOST`.
- Lệnh `get_engine_install_status` cho biết đã cài chưa; **Giới thiệu → Chẩn đoán** hiện phiên bản.

**Hồ sơ cũ (đã tạo bằng Wayfern) tự chuyển sang Apostate.**
- Lúc khởi động (và khi mở nếu sót), Donut ghi `engine = apostate` cho hồ sơ, đặt phiên bản về Chromium của engine, giữ nguyên thư mục dữ liệu nên cookie và đăng nhập còn nguyên.
- Hồ sơ cũ lưu thiết bị sau một `identity_id` (Wayfern sinh, Donut không giữ nội dung) được bốc lại một thiết bị cục bộ từ chính id hồ sơ, rồi áp các chỉnh sửa của người dùng lên trên; xóa `identity_id`.
- Việc chuyển không phải là sửa hồ sơ: không đổi `updated_at`, không ghi nhật ký nhóm, nên không máy nào đẩy lên một thay đổi mà các máy khác không tự làm được.

## Sinh vân tay cục bộ

Hồ sơ mới không gọi Wayfern và không cần token nào. Donut tự dựng thiết bị (`src-tauri/src/fingerprint_generator.rs`): User-Agent, platform, màn hình, CPU/RAM, WebGL, font, voice, audio, battery, … theo OS khai báo, rồi ghi toàn bộ vào `wayfern_config.fingerprint`. Khi mở, Apostate nhận thiết bị qua dòng lệnh (`--fingerprint-*`); không có API danh tính.

- Cùng một profile id luôn ra cùng một thiết bị; hai hồ sơ khác nhau ra hai thiết bị khác nhau.
- Màn hình, GPU, số nhân CPU và RAM được bốc theo tần suất thật của máy để bàn (`src-tauri/src/fingerprint_devices.json`, lấy từ dữ liệu của Apify fingerprint-suite, giấy phép Apache-2.0): khoảng 40 cỡ màn hình và 30–90 GPU mỗi OS. Donut tự ghép cho hợp lý: chip Apple M chỉ đi với số nhân nó có (M1 = 8, M4 = 10, ...), RAM khai tối đa 8 GB như Chrome, và GPU cùng hãng với GPU thật của máy vì WebGL vẫn vẽ bằng GPU thật.
- Múi giờ, ngôn ngữ và tọa độ lấy từ lối ra của hồ sơ (proxy/VPN) lúc tạo, không lấy từ máy. Nếu không đo được lối ra mà hồ sơ có proxy/VPN, các trường vị trí bị bỏ trống để lần đo sau điền lại.
- Chỉ hỗ trợ Windows/macOS/Linux, kể cả OS trùng với máy chạy app. Android/iOS không còn trong danh sách chọn OS, và backend từ chối bằng `FINGERPRINT_MOBILE_UNSUPPORTED`.
- WebGPU, Client Hints, font và TLS/p0f do Apostate kiểm soát ở mức trình duyệt; p0f/TCP-IP vẫn phụ thuộc OS của máy và lối ra.

## Hồ sơ khai OS khác máy (ví dụ Windows trên Mac)

- **Không có nút chọn engine**, chỉ có một engine. Danh sách OS trong hộp thoại tạo hồ sơ có đủ Windows/macOS/Linux.
- **Mở được khi không có proxy/VPN**, kèm cảnh báo lúc tạo hồ sơ: tầng TCP/mạng vẫn lộ OS của máy nếu không đi qua WireGuard. Hồ sơ đã gắn proxy/VPN mà lối đó hỏng thì vẫn bị chặn (`FINGERPRINT_HOST_OS_MISMATCH`). "Thị trường" (khóa nước lối ra) giữ nguyên hành vi.
- **Mac Apple silicon khai báo Windows-on-ARM.** Apostate dựng thiết bị Windows ARM (GPU Adreno) chứ không phải Windows x64 với card NVIDIA/Intel, vì đó là loại phần cứng Mac có thể giả gần nhất. Donut chỉ truyền hạt giống (id hồ sơ), OS, ngôn ngữ và múi giờ.
- **Font.** Máy Windows không cần cài gì. Hồ sơ cùng OS với máy cũng không cần. Hồ sơ Windows trên Mac hoặc Linux cần font Windows 11 trên máy; thiếu Segoe UI Variable và Sitka thì hồ sơ khai Windows 11 mà mang bộ font Windows 10, iphey báo "inconsistent browser fingerprint (butterfly)". Donut **tự cài** các font này (`src-tauri/src/persona_fonts.rs`): sau khi tải engine, nó tải nền khoảng 357 MB (142 tệp của các họ font lõi Windows 11 từ một commit cố định của `MauCariApa-com/windows-11-fonts`, kiểm SHA-256 từng tệp, cộng Marlett đọc theo byte range từ `liblaf/fonts`) vào `~/Library/Fonts/apostate-windows` (Mac) hoặc `~/.local/share/fonts/apostate-windows` (Linux, kèm `fc-cache`), có toast tiến độ. Mở hồ sơ Windows khi font chưa xong thì lần mở đó chờ cùng lượt tải; tải lỗi thì vẫn mở (log `Persona fonts not installed`) và nền tự thử lại. Thư mục này trùng với lệnh `apostate fonts install windows`, nên font đã cài tay vẫn được tính. Font macOS cho hồ sơ Mac trên Windows phải lấy từ một máy Mac thật (`apostate fonts export-macos`).
- **Widevine tắt.** Donut mở Apostate với `--disable-component-update`: CDM Widevine tự tải về trả lời EME theo OS thật của máy (iphey báo "whale"), cờ này cũng chặn CDM đã tải ở hồ sơ cũ. Đổi lại không xem được video có DRM (Netflix, ...).
- **Tính năng chỉ có ở Wayfern** (agent, ghi recipe, synchronizer, bộ chọn phần tử của MCP dựa trên miền `Wayfern.*`, xem/làm mới danh tính) bị ẩn khỏi giao diện hoặc từ chối bằng `APOSTATE_FEATURE_UNSUPPORTED`.

Những gì vẫn còn lộ:

- WebGL/WebGPU vẫn vẽ bằng GPU Apple thật nên giới hạn và kết quả vẽ không giống hẳn Adreno.
- Số nhân CPU và RAM của máy thật là trần; hồ sơ không khai nhiều hơn máy có.
- Camera và micro của Mac vẫn là thiết bị người dùng thấy.
- p0f/TCP-IP vẫn là của máy chạy app, không phải của OS khai báo (trừ khi đi qua WireGuard của Donut, nơi Donut gán TTL và cửa sổ TCP theo OS khai báo).

## Phụ thuộc vào máy chủ bên ngoài

Khi dùng tài khoản tự host, ứng dụng chỉ gọi ra ngoài các địa chỉ sau:

| Địa chỉ | Để làm gì | Thay được không |
|---|---|---|
| `github.com/leduyphuc1702/donutbrowser` | Kiểm tra và tải bản cập nhật | Đã là repo của Vietflow |
| `github.com/heretic-tech/apostate/releases` | Tải engine Apostate (mã nguồn mở) ở lần mở đầu tiên; bản và sha256 được ghim trong `src-tauri/src/apostate.rs` | Có thể mirror, nhưng phải đổi `RELEASE_BASE`. Đây là phụ thuộc cứng duy nhất để mở trình duyệt; sau khi tải xong không cần mạng nữa |
| GitHub (cơ sở dữ liệu GeoIP) | Định vị IP lối ra | Có thể tự host, chưa cần |
| Control plane của Vietflow | Tài khoản, entitlements, khóa hồ sơ | Đã tự host (`vietflow/control-plane/`) |
| Máy chủ đồng bộ của Vietflow | Đồng bộ hồ sơ và khóa phiên | Đã tự host (`vietflow/sync-server/`) |

`api.donutbrowser.com` và `sync.donutbrowser.com` chỉ còn được gọi khi cố ý dùng tài khoản Donut cloud. Khi dùng control plane tự host, hồ sơ mới sinh thiết bị cục bộ và engine là Apostate mã nguồn mở nên không cần token hay điều khoản của bên thứ ba nào.

Khi chưa đăng nhập control plane, vẫn còn giới hạn cũ: đang mở **hơn 3 trình duyệt** cùng lúc thì Donut không tạo được hồ sơ mới (việc sinh thiết bị cần mở thêm một trình duyệt ngầm). Đăng nhập tài khoản tự host để bỏ giới hạn này.

## Phát hành bản build (Windows và macOS)

GitHub **tắt Actions trên fork** của repo lớn, nên `.github/workflows/vietflow-release.yml` không chạy được ở `leduyphuc1702/donutbrowser`. Đường đang dùng:

1. Repo build riêng, không phải fork: `leduyphuc1702/donut-build`, chứa cùng mã nguồn và một workflow `Build installers` (`.github/workflows/build-artifacts.yml`). Workflow này chỉ tạo artifact, không tạo release.
2. Chạy workflow đó với tag phiên bản, ví dụ `v0.36.1`; `only` để trống thì build cả ba gói, hoặc điền `x86_64-apple-darwin` để build lại đúng một gói.
3. Tải artifact về máy rồi tạo release ở repo chính: `gh release create` + `gh release upload` bốn tệp `.dmg`/`-setup.exe`/`-portable.zip` và `SHA256SUMS.txt`.

Tên tệp phải giữ đúng `Donut_<version>_aarch64.dmg`, `Donut_<version>_x64.dmg`, `Donut_<version>_x64-setup.exe`: `src-tauri/src/app_auto_updater.rs` chọn bản cập nhật theo đúng các mẫu đó, và từ chối release không có `SHA256SUMS.txt`.

Bản macOS ký ad hoc (`APPLE_SIGNING_IDENTITY="-"`), người dùng mở lần đầu bằng chuột phải → Open. Trên macOS Intel, trình liên kết không tự ký nên workflow đặt `RUSTFLAGS=-C link-arg=-Wl,-adhoc_codesign`; thiếu nó thì `donut-proxy` trong `Contents/MacOS` không có chữ ký và bước đóng gói hỏng với `code object is not signed at all`.
