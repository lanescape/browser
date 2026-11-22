#ifndef LANESCAPE_BROWSER_BROWSER_H_
#define LANESCAPE_BROWSER_BROWSER_H_

#include <map>
#include <memory>
#include <string>

#include "base/files/file_path.h"
#include "base/containers/span.h"
#include "base/functional/callback.h"
#include "base/memory/raw_ptr.h"
#include "base/memory/raw_ref.h"
#include "base/memory/weak_ptr.h"
#include "content/public/browser/browser_context.h"
#include "content/public/browser/browser_main_parts.h"
#include "content/public/browser/global_routing_id.h"
#include "engine/clipboard.h"
#include "engine/lanescape_browser.h"
#include "engine/lanescape_browser.mojom.h"
#include "mojo/public/cpp/bindings/associated_receiver_set.h"
#include "mojo/public/cpp/bindings/pending_associated_receiver.h"

namespace base {
class DictValue;
class RunLoop;
}

namespace content {
class RenderFrameHost;
class WebContents;
}

namespace gfx {
class Rect;
class Size;
}

namespace lanescape_browser {

class DevToolsSession;
class Tab;

void SetHost(const LanescapeBrowserHost& host);
// Where every profile's storage is, as the host was launched with it. Set before Chromium starts.
void SetDataDirectory(std::string path);
const std::string& DataDirectory();
// Sends a JSON event to the host. UI thread only.
void Emit(const std::string& json);
// Hands a frame to the host. UI thread only.
void DeliverFrame(const LanescapeBrowserFrame& frame);
// A recording's next frame, or the end of it when `data` is empty and `finished`. Any thread.
void DeliverRecordingFrame(uint64_t recording,
                           base::span<const uint8_t> data,
                           int64_t pts,
                           bool key,
                           uint32_t width,
                           uint32_t height);
void DeliverRecordingEnd(uint64_t recording);
// Hands a DevTools message to the host. UI thread only.
void DeliverDevTools(uint64_t session, base::span<const uint8_t> message);
// Queues a JSON command for the UI thread. Any thread.
void PostCommand(std::string json);
// Queues a DevTools message for a session on the UI thread. Any thread.
void PostDevTools(uint64_t session,
                  uint64_t id,
                  bool is_input,
                  std::string message);

// Takes what a frame's renderer reports. UI thread only.
void BindFrameHost(content::GlobalRenderFrameHostId frame,
                   mojo::PendingAssociatedReceiver<mojom::FrameHost> receiver);

// One browsing context: a persistent profile, or a temporary one with an empty path.
class BrowserContext : public content::BrowserContext {
 public:
  explicit BrowserContext(base::FilePath path);
  ~BrowserContext() override;

  base::FilePath GetPath() const override;
  std::unique_ptr<content::ZoomLevelDelegate> CreateZoomLevelDelegate(
      const base::FilePath& partition_path) override;
  bool IsOffTheRecord() override;
  content::DownloadManagerDelegate* GetDownloadManagerDelegate() override;
  content::BrowserPluginGuestManager* GetGuestManager() override;
  storage::SpecialStoragePolicy* GetSpecialStoragePolicy() override;
  content::PlatformNotificationService* GetPlatformNotificationService()
      override;
  content::PushMessagingService* GetPushMessagingService() override;
  content::StorageNotificationService* GetStorageNotificationService() override;
  content::SSLHostStateDelegate* GetSSLHostStateDelegate() override;
  content::PermissionControllerDelegate* GetPermissionControllerDelegate()
      override;
  content::ClientHintsControllerDelegate* GetClientHintsControllerDelegate()
      override;
  content::BackgroundFetchDelegate* GetBackgroundFetchDelegate() override;
  content::BackgroundSyncController* GetBackgroundSyncController() override;
  content::BrowsingDataRemoverDelegate* GetBrowsingDataRemoverDelegate()
      override;
  content::ReduceAcceptLanguageControllerDelegate*
  GetReduceAcceptLanguageControllerDelegate() override;
  content::OriginTrialsControllerDelegate* GetOriginTrialsControllerDelegate()
      override;

  ContextClipboard& clipboard() { return clipboard_; }

 private:
  const base::FilePath path_;
  ContextClipboard clipboard_;
  Permissions permissions_;
};

class BrowserMainParts : public content::BrowserMainParts,
                         public mojom::FrameHost {
 public:
  BrowserMainParts();
  ~BrowserMainParts() override;

  void ToolkitInitialized() override;
  int PreMainMessageLoopRun() override;
  void WillRunMainMessageLoop(
      std::unique_ptr<base::RunLoop>& run_loop) override;
  void PostMainMessageLoopRun() override;

  void HandleCommand(const std::string& json);

  // Takes a popup's contents as a new tab in its opener's context.
  content::WebContents* AdoptPopup(Tab& opener,
                                   std::unique_ptr<content::WebContents> contents,
                                   const gfx::Size& size);
  // Whether another tab fits the host's bound, counting popups Chromium has created and not yet
  // handed over.
  bool HasRoomForTab() const;
  void PopupCreating(content::WebContents* contents);
  // Closes a tab from outside its own call stack.
  void CloseTabSoon(int id);
  int NextDialog() { return next_dialog_++; }

  void BindFrame(content::GlobalRenderFrameHostId frame,
                 mojo::PendingAssociatedReceiver<mojom::FrameHost> receiver);

  // mojom::FrameHost:
  void FocusChanged(int32_t counter,
                    bool editable,
                    const gfx::Rect& bounds) override;
  void CommandStarted(int32_t token) override;
  void CommandFinished(int32_t token,
                       mojom::CommandEnd end,
                       bool selection,
                       bool editable) override;

 private:
  friend void PostDevTools(uint64_t session,
                           uint64_t id,
                           bool is_input,
                           std::string message);

  class GpuStatusClient;

  Tab* FindTab(const base::DictValue& command);
  // The frame sending the current FrameHost message and its tab, if both are still here.
  Tab* TabOfCurrentFrame(content::RenderFrameHost** frame);
  void SendDevTools(uint64_t session,
                    uint64_t id,
                    bool is_input,
                    const std::string& message);
  BrowserContext* ContextFor(const std::string& key, const std::string* path);
  void FlushThen(const std::string* context, base::OnceClosure done);
  void CloseTab(int id);
  void Quit();

  std::map<std::string, std::unique_ptr<BrowserContext>> contexts_;
  std::map<int, std::unique_ptr<Tab>> tabs_;
  class PendingPopup;
  std::map<content::WebContents*, std::unique_ptr<PendingPopup>> pending_popups_;
  std::map<uint64_t, std::unique_ptr<DevToolsSession>> devtools_;
  std::unique_ptr<GpuStatusClient> gpu_status_;
  int next_tab_ = 1;
  int next_dialog_ = 1;
  base::OnceClosure quit_;
  mojo::AssociatedReceiverSet<mojom::FrameHost,
                              content::GlobalRenderFrameHostId>
      frame_hosts_;
  base::WeakPtrFactory<BrowserMainParts> weak_factory_{this};
};

}  // namespace lanescape_browser

#endif  // LANESCAPE_BROWSER_BROWSER_H_
