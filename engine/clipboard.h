#ifndef LANESCAPE_BROWSER_CLIPBOARD_H_
#define LANESCAPE_BROWSER_CLIPBOARD_H_

#include <memory>
#include <optional>
#include <string>

#include "base/functional/callback.h"
#include "content/public/browser/global_routing_id.h"
#include "content/public/browser/permission_controller_delegate.h"
#include "mojo/public/cpp/bindings/pending_receiver.h"
#include "third_party/blink/public/mojom/clipboard/clipboard.mojom-forward.h"

namespace content {
class RenderFrameHost;
}

namespace ui {
class Clipboard;
}

namespace lanescape_browser {

// One browsing context's clipboard, which holds data only while an operation runs and only for the
// frame running it. Everything else a page does goes nowhere: a write outside an operation is
// discarded, and a read finds nothing.
class ContextClipboard {
 public:
  // Text is at most this many UTF-8 bytes (specs/0002-control.md).
  static constexpr size_t kTextLimit = 1 << 20;

  // What a copy's frame committed.
  struct Written {
    std::u16string text;
    // Text above the limit, which is not kept.
    bool too_large = false;
    // Any format that isn't text.
    bool other = false;
  };

  ContextClipboard();
  ~ContextClipboard();

  // The frame's command is about to run, replacing any operation still here. A copy collects what
  // the frame writes; a paste offers `text` to it.
  void Begin(int token,
             content::GlobalRenderFrameHostId frame,
             bool copies,
             std::u16string text);
  // Ends that operation, if it's the one here.
  void End(int token);
  bool Runs(int token) const { return token_ == token && token != 0; }
  bool RunsIn(int token, content::GlobalRenderFrameHostId frame) const {
    return Runs(token) && frame_ == frame;
  }
  bool copies() const { return copies_; }
  // Runs `done` when the frame commits a write, or now if it has.
  void WhenCommitted(base::OnceClosure done);
  bool committed() const { return committed_.has_value(); }
  // The last write committed, if any.
  const std::optional<Written>& written() const { return committed_; }

  // The page's side, for one frame.
  const std::u16string* Offered(content::GlobalRenderFrameHostId frame) const;
  int token() const { return token_; }
  void WriteText(content::GlobalRenderFrameHostId frame,
                 const std::u16string& text);
  void WriteOther(content::GlobalRenderFrameHostId frame);
  void Commit(content::GlobalRenderFrameHostId frame);

 private:
  bool Collecting(content::GlobalRenderFrameHostId frame) const;

  int token_ = 0;
  content::GlobalRenderFrameHostId frame_;
  bool copies_ = false;
  std::u16string offered_;
  Written writing_;
  std::optional<Written> committed_;
  base::OnceClosure on_commit_;
};

// Overrides Content's frame binder.
void BindPageClipboard(
    content::RenderFrameHost* frame,
    mojo::PendingReceiver<blink::mojom::ClipboardHost> receiver);

// For the one thing still reaching ui::Clipboard. Content binds its own ClipboardHost for service
// workers, and an embedder can't override that; with this, it reads and keeps nothing.
std::unique_ptr<ui::Clipboard> CreateInertClipboard();

// Pages never get the async Clipboard API's read permission, without a prompt or any stored
// decision. Writing what they were asked to needs a user gesture, and goes through the clipboard
// above. Nothing else is granted, as with no delegate at all.
class Permissions : public content::PermissionControllerDelegate {
 public:
  void RequestPermissionsFromCurrentDocument(
      content::RenderFrameHost* render_frame_host,
      const content::PermissionRequestDescription& request_description,
      base::OnceCallback<void(const std::vector<content::PermissionResult>&)>
          callback) override;
  content::PermissionStatus GetPermissionStatus(
      const blink::mojom::PermissionDescriptorPtr& permission,
      const GURL& requesting_origin,
      const GURL& embedding_origin) override;
  content::PermissionResult GetPermissionResultForOriginWithoutContext(
      const blink::mojom::PermissionDescriptorPtr& permission_descriptor,
      const url::Origin& requesting_origin,
      const url::Origin& embedding_origin) override;
  content::PermissionResult GetPermissionResultForCurrentDocument(
      const blink::mojom::PermissionDescriptorPtr& permission_descriptor,
      content::RenderFrameHost* render_frame_host,
      bool should_include_device_status) override;
  content::PermissionResult GetPermissionResultForWorker(
      const blink::mojom::PermissionDescriptorPtr& permission_descriptor,
      content::RenderProcessHost* render_process_host,
      const GURL& worker_origin) override;
  content::PermissionResult GetPermissionResultForEmbeddedRequester(
      const blink::mojom::PermissionDescriptorPtr& permission_descriptor,
      content::RenderFrameHost* render_frame_host,
      const url::Origin& requesting_origin) override;
  void ResetPermission(blink::PermissionType permission,
                       const GURL& requesting_origin,
                       const GURL& embedding_origin) override;
};

}  // namespace lanescape_browser

#endif  // LANESCAPE_BROWSER_CLIPBOARD_H_
