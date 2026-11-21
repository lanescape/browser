#include "engine/clipboard.h"

#include <utility>
#include <vector>

#include "base/containers/flat_map.h"
#include "base/containers/flat_set.h"
#include "base/functional/callback_helpers.h"
#include "build/build_config.h"
#include "content/public/browser/document_service.h"
#include "content/public/browser/permission_request_description.h"
#include "content/public/browser/permission_result.h"
#include "content/public/browser/render_frame_host.h"
#include "engine/browser.h"
#include "mojo/public/cpp/base/big_buffer.h"
#include "third_party/abseil-cpp/absl/numeric/int128.h"
#include "third_party/blink/public/common/permissions/permission_utils.h"
#include "third_party/blink/public/mojom/clipboard/clipboard.mojom.h"
#include "third_party/blink/public/mojom/permissions/permission_status.mojom.h"
#include "third_party/skia/include/core/SkBitmap.h"
#include "ui/base/clipboard/clipboard.h"
#include "ui/base/clipboard/clipboard_buffer.h"
#include "ui/base/clipboard/clipboard_format_type.h"
#include "ui/base/clipboard/clipboard_sequence_number_token.h"
#include "ui/base/clipboard/file_info.h"
#include "ui/base/data_transfer_policy/data_transfer_endpoint.h"
#include "url/gurl.h"

namespace lanescape_browser {

ContextClipboard::ContextClipboard() = default;
ContextClipboard::~ContextClipboard() = default;

void ContextClipboard::Begin(int token,
                             content::GlobalRenderFrameHostId frame,
                             bool copies,
                             std::u16string text) {
  token_ = token;
  frame_ = frame;
  copies_ = copies;
  offered_ = std::move(text);
  writing_ = {};
  committed_.reset();
  on_commit_.Reset();
}

void ContextClipboard::End(int token) {
  if (Runs(token)) {
    Begin(0, {}, false, {});
  }
}

void ContextClipboard::WhenCommitted(base::OnceClosure done) {
  if (committed_) {
    std::move(done).Run();
  } else {
    on_commit_ = std::move(done);
  }
}

const std::u16string* ContextClipboard::Offered(
    content::GlobalRenderFrameHostId frame) const {
  return token_ != 0 && !copies_ && frame_ == frame ? &offered_ : nullptr;
}

bool ContextClipboard::Collecting(
    content::GlobalRenderFrameHostId frame) const {
  return token_ != 0 && copies_ && frame_ == frame;
}

void ContextClipboard::WriteText(content::GlobalRenderFrameHostId frame,
                                 const std::u16string& text) {
  if (!Collecting(frame)) {
    return;
  }
  // A UTF-16 unit is at least one UTF-8 byte, so more units than the limit are already past it,
  // and aren't kept. The limit in bytes is measured once committed.
  writing_.too_large = text.size() > kTextLimit;
  writing_.text = writing_.too_large ? std::u16string() : text;
}

void ContextClipboard::WriteOther(content::GlobalRenderFrameHostId frame) {
  if (Collecting(frame)) {
    writing_.other = true;
  }
}

void ContextClipboard::Commit(content::GlobalRenderFrameHostId frame) {
  if (!Collecting(frame)) {
    return;
  }
  committed_ = std::exchange(writing_, Written());
  if (on_commit_) {
    std::move(on_commit_).Run();
  }
}

namespace {

// A frame's Blink clipboard, whose writes and reads stop at its context's clipboard.
class PageClipboard final
    : public content::DocumentService<blink::mojom::ClipboardHost> {
 public:
  PageClipboard(content::RenderFrameHost& frame,
                mojo::PendingReceiver<blink::mojom::ClipboardHost> receiver)
      : DocumentService(frame, std::move(receiver)),
        clipboard_(static_cast<BrowserContext*>(frame.GetBrowserContext())
                       ->clipboard()),
        frame_(frame.GetGlobalId()) {}

  // blink::mojom::ClipboardHost:
  void GetSequenceNumber(ui::ClipboardBuffer buffer,
                         GetSequenceNumberCallback callback) override {
    // A paste's data is good for its operation; between operations there is none to go stale.
    std::move(callback).Run(
        absl::MakeUint128(0, Offered(buffer) ? clipboard_->token() : 0));
  }
  void IsFormatAvailable(blink::mojom::ClipboardFormat format,
                         ui::ClipboardBuffer buffer,
                         IsFormatAvailableCallback callback) override {
    std::move(callback).Run(format ==
                                blink::mojom::ClipboardFormat::kPlaintext &&
                            Offered(buffer));
  }
  void ReadAvailableTypes(ui::ClipboardBuffer buffer,
                          ReadAvailableTypesCallback callback) override {
    std::vector<std::u16string> types;
    if (Offered(buffer)) {
      types.push_back(u"text/plain");
    }
    std::move(callback).Run(std::move(types));
  }
  void ReadText(ui::ClipboardBuffer buffer,
                ReadTextCallback callback) override {
    const std::u16string* offered = Offered(buffer);
    std::move(callback).Run(offered ? *offered : std::u16string());
  }
  void ReadHtml(ui::ClipboardBuffer buffer,
                ReadHtmlCallback callback) override {
    std::move(callback).Run(std::u16string(), GURL(), 0, 0);
  }
  void ReadSvg(ui::ClipboardBuffer buffer, ReadSvgCallback callback) override {
    std::move(callback).Run(std::u16string());
  }
  void ReadRtf(ui::ClipboardBuffer buffer, ReadRtfCallback callback) override {
    std::move(callback).Run(std::string());
  }
  void ReadPng(ui::ClipboardBuffer buffer, ReadPngCallback callback) override {
    std::move(callback).Run(mojo_base::BigBuffer());
  }
  void ReadFiles(ui::ClipboardBuffer buffer,
                 ReadFilesCallback callback) override {
    std::move(callback).Run(blink::mojom::ClipboardFiles::New());
  }
  void ReadDataTransferCustomData(
      ui::ClipboardBuffer buffer,
      const std::u16string& type,
      ReadDataTransferCustomDataCallback callback) override {
    std::move(callback).Run(std::u16string());
  }
  void ReadAvailableCustomAndStandardFormats(
      ReadAvailableCustomAndStandardFormatsCallback callback) override {
    std::move(callback).Run({});
  }
  void ReadUnsanitizedCustomFormat(
      const std::u16string& format,
      ReadUnsanitizedCustomFormatCallback callback) override {
    std::move(callback).Run(mojo_base::BigBuffer());
  }
  void WriteText(const std::u16string& text) override {
    clipboard_->WriteText(frame_, text);
  }
  void WriteHtml(const std::u16string& markup, const GURL& url) override {
    clipboard_->WriteOther(frame_);
  }
  void WriteSvg(const std::u16string& markup) override {
    clipboard_->WriteOther(frame_);
  }
  void WriteSmartPasteMarker() override {}
  void WriteDataTransferCustomData(
      const base::flat_map<std::u16string, std::u16string>& data) override {
    clipboard_->WriteOther(frame_);
  }
  void WriteBookmark(const std::string& url,
                     const std::u16string& title) override {
    clipboard_->WriteOther(frame_);
  }
  void WriteImage(const SkBitmap& image) override {
    clipboard_->WriteOther(frame_);
  }
  void WriteUnsanitizedCustomFormat(const std::u16string& format,
                                    mojo_base::BigBuffer data) override {
    clipboard_->WriteOther(frame_);
  }
  void CommitWrite() override { clipboard_->Commit(frame_); }
#if BUILDFLAG(IS_MAC)
  // macOS's find pasteboard is the machine's own, which no page may touch.
  void WriteStringToFindPboard(const std::u16string& text) override {}
  // Reads without a person's action are refused, whatever the machine's settings.
  void GetPlatformPermissionState(
      GetPlatformPermissionStateCallback callback) override {
    std::move(callback).Run(blink::mojom::PlatformClipboardPermissionState::kDeny);
  }
#endif
  // Nothing tells a page the clipboard changed: it never does, from where a page stands.
  void RegisterClipboardListener(
      mojo::PendingRemote<blink::mojom::ClipboardListener> listener) override {}

 private:
  const std::u16string* Offered(ui::ClipboardBuffer buffer) const {
    return buffer == ui::ClipboardBuffer::kCopyPaste
               ? clipboard_->Offered(frame_)
               : nullptr;
  }

  // The context outlives every document in it.
  const raw_ref<ContextClipboard> clipboard_;
  const content::GlobalRenderFrameHostId frame_;
};

class InertClipboard final : public ui::Clipboard {
 public:
  void OnPreShutdown() override {}
  void GetSource(ui::ClipboardBuffer buffer,
                 GetSourceCallback callback) const override {
    std::move(callback).Run(std::nullopt);
  }
  const ui::ClipboardSequenceNumberToken& GetSequenceNumber(
      ui::ClipboardBuffer buffer) const override {
    return sequence_;
  }
  void GetStandardFormats(ui::ClipboardBuffer buffer,
                          const std::optional<ui::DataTransferEndpoint>& data_dst,
                          GetStandardFormatsCallback callback) const override {
    std::move(callback).Run({});
  }
  void GetAllAvailableFormats(
      ui::ClipboardBuffer buffer,
      const std::optional<ui::DataTransferEndpoint>& data_dst,
      base::OnceCallback<void(base::flat_set<ui::ClipboardFormatType>)> callback)
      const override {
    std::move(callback).Run({});
  }
  void GetAvailableFormats(
      ui::ClipboardBuffer buffer,
      std::vector<ui::ClipboardFormatType> formats,
      const std::optional<ui::DataTransferEndpoint>& data_dst,
      base::OnceCallback<void(base::flat_set<ui::ClipboardFormatType>)> callback)
      const override {
    std::move(callback).Run({});
  }
  void Clear(ui::ClipboardBuffer buffer) override {}
  void ReadAvailableTypes(ui::ClipboardBuffer buffer,
                          const std::optional<ui::DataTransferEndpoint>& data_dst,
                          ReadAvailableTypesCallback callback) const override {
    std::move(callback).Run({});
  }
  void ReadText(ui::ClipboardBuffer buffer,
                const std::optional<ui::DataTransferEndpoint>& data_dst,
                ReadTextCallback callback) const override {
    std::move(callback).Run(std::u16string());
  }
  void ReadAsciiText(ui::ClipboardBuffer buffer,
                     const std::optional<ui::DataTransferEndpoint>& data_dst,
                     ReadAsciiTextCallback callback) const override {
    std::move(callback).Run(std::string());
  }
  void ReadHTML(ui::ClipboardBuffer buffer,
                const std::optional<ui::DataTransferEndpoint>& data_dst,
                ReadHtmlCallback callback) const override {
    std::move(callback).Run(std::u16string(), GURL(), 0, 0);
  }
  void ReadSvg(ui::ClipboardBuffer buffer,
               const std::optional<ui::DataTransferEndpoint>& data_dst,
               ReadSvgCallback callback) const override {
    std::move(callback).Run(std::u16string());
  }
  void ReadRTF(ui::ClipboardBuffer buffer,
               const std::optional<ui::DataTransferEndpoint>& data_dst,
               ReadRTFCallback callback) const override {
    std::move(callback).Run(std::string());
  }
  void ReadPng(ui::ClipboardBuffer buffer,
               const std::optional<ui::DataTransferEndpoint>& data_dst,
               ReadPngCallback callback) const override {
    std::move(callback).Run({});
  }
  void ReadDataTransferCustomData(
      ui::ClipboardBuffer buffer,
      const std::u16string& type,
      const std::optional<ui::DataTransferEndpoint>& data_dst,
      ReadDataTransferCustomDataCallback callback) const override {
    std::move(callback).Run(std::u16string());
  }
  void ReadFilenames(ui::ClipboardBuffer buffer,
                     const std::optional<ui::DataTransferEndpoint>& data_dst,
                     ReadFilenamesCallback callback) const override {
    std::move(callback).Run({});
  }
  void ReadURL(const std::optional<ui::DataTransferEndpoint>& data_dst,
               ReadUrlCallback callback) const override {
    std::move(callback).Run({});
  }
  void ReadData(const ui::ClipboardFormatType& format,
                const std::optional<ui::DataTransferEndpoint>& data_dst,
                ReadDataCallback callback) const override {
    std::move(callback).Run(std::string());
  }
  void WritePortableAndPlatformRepresentations(
      ui::ClipboardBuffer buffer,
      const ObjectMap& objects,
      const std::vector<RawData>& raw_objects,
      std::vector<Clipboard::PlatformRepresentation> platform_representations,
      std::unique_ptr<ui::DataTransferEndpoint> data_src,
      uint32_t privacy_types) override {}
  void WriteText(std::string_view text) override {}
  void WriteHTML(std::string_view markup,
                 std::optional<std::string_view> source_url) override {}
  void WriteSvg(std::string_view markup) override {}
  void WriteRTF(std::string_view rtf) override {}
  void WriteFilenames(std::vector<ui::FileInfo> filenames) override {}
  void WriteURL(const ui::ClipboardUrlInfo& url_info) override {}
  void WriteWebSmartPaste() override {}
  void WriteBitmap(const SkBitmap& bitmap) override {}
  void WriteData(const ui::ClipboardFormatType& format,
                 base::span<const uint8_t> data) override {}
#if BUILDFLAG(IS_OZONE)
  bool IsSelectionBufferAvailable() const override { return false; }
#endif

 private:
  const ui::ClipboardSequenceNumberToken sequence_;
};

blink::mojom::PermissionStatus StatusOf(
    const blink::mojom::PermissionDescriptorPtr& permission,
    bool gesture) {
  return gesture && blink::MaybePermissionDescriptorToPermissionType(permission) ==
                        blink::PermissionType::CLIPBOARD_SANITIZED_WRITE
             ? blink::mojom::PermissionStatus::GRANTED
             : blink::mojom::PermissionStatus::DENIED;
}

// What a page asks without acting: a write is allowed once it acts, as in Chrome.
content::PermissionResult Query(
    const blink::mojom::PermissionDescriptorPtr& permission) {
  return content::PermissionResult(StatusOf(permission, /*gesture=*/true));
}

}  // namespace

void BindPageClipboard(
    content::RenderFrameHost* frame,
    mojo::PendingReceiver<blink::mojom::ClipboardHost> receiver) {
  // A DocumentService deletes itself with its document or connection.
  new PageClipboard(*frame, std::move(receiver));
}

std::unique_ptr<ui::Clipboard> CreateInertClipboard() {
  return std::make_unique<InertClipboard>();
}

void Permissions::RequestPermissionsFromCurrentDocument(
    content::RenderFrameHost* render_frame_host,
    const content::PermissionRequestDescription& request_description,
    base::OnceCallback<void(const std::vector<content::PermissionResult>&)>
        callback) {
  std::vector<content::PermissionResult> results;
  for (const blink::mojom::PermissionDescriptorPtr& permission :
       request_description.permissions) {
    results.emplace_back(
        StatusOf(permission, request_description.user_gesture));
  }
  std::move(callback).Run(results);
}

content::PermissionStatus Permissions::GetPermissionStatus(
    const blink::mojom::PermissionDescriptorPtr& permission,
    const GURL& requesting_origin,
    const GURL& embedding_origin) {
  return StatusOf(permission, /*gesture=*/true);
}

content::PermissionResult Permissions::GetPermissionResultForOriginWithoutContext(
    const blink::mojom::PermissionDescriptorPtr& permission_descriptor,
    const url::Origin& requesting_origin,
    const url::Origin& embedding_origin) {
  return Query(permission_descriptor);
}

content::PermissionResult Permissions::GetPermissionResultForCurrentDocument(
    const blink::mojom::PermissionDescriptorPtr& permission_descriptor,
    content::RenderFrameHost* render_frame_host,
    bool should_include_device_status) {
  return Query(permission_descriptor);
}

content::PermissionResult Permissions::GetPermissionResultForWorker(
    const blink::mojom::PermissionDescriptorPtr& permission_descriptor,
    content::RenderProcessHost* render_process_host,
    const GURL& worker_origin) {
  return Query(permission_descriptor);
}

content::PermissionResult Permissions::GetPermissionResultForEmbeddedRequester(
    const blink::mojom::PermissionDescriptorPtr& permission_descriptor,
    content::RenderFrameHost* render_frame_host,
    const url::Origin& requesting_origin) {
  return Query(permission_descriptor);
}

void Permissions::ResetPermission(blink::PermissionType permission,
                                  const GURL& requesting_origin,
                                  const GURL& embedding_origin) {}

}  // namespace lanescape_browser
