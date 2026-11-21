#ifndef LANESCAPE_BROWSER_TAB_H_
#define LANESCAPE_BROWSER_TAB_H_

#include <deque>
#include <map>
#include <memory>
#include <optional>
#include <string>
#include <vector>

#include "base/containers/flat_map.h"
#include "base/containers/flat_set.h"
#include "base/functional/callback_helpers.h"
#include "base/memory/raw_ptr.h"
#include "base/memory/raw_ref.h"
#include "base/memory/read_only_shared_memory_region.h"
#include "base/memory/weak_ptr.h"
#include "base/observer_list.h"
#include "base/time/time.h"
#include "base/timer/timer.h"
#include "base/unguessable_token.h"
#include "base/values.h"
#include "components/viz/host/client_frame_sink_video_capturer.h"
#include "content/public/browser/global_routing_id.h"
#include "content/public/browser/javascript_dialog_manager.h"
#include "content/public/browser/render_widget_host.h"
#include "content/public/browser/web_contents_delegate.h"
#include "content/public/browser/web_contents_observer.h"
#include "engine/lanescape_browser.mojom.h"
#include "engine/tab_view.h"
#include "mojo/public/cpp/bindings/associated_remote.h"
#include "services/viz/privileged/mojom/compositing/frame_sink_video_capture.mojom.h"
#include "ui/events/keycodes/keyboard_codes.h"
#include "ui/gfx/geometry/rect.h"
#include "ui/gfx/geometry/size.h"

class GURL;

namespace content {
class BrowserContext;
class WebContents;
struct ContextMenuParams;
}  // namespace content

namespace lanescape_browser {

class BrowserMainParts;
class ContextClipboard;
class Recorder;

// One browser tab, its input and its pixel capture.
class Tab : public content::WebContentsObserver,
            public content::WebContentsDelegate,
            public content::JavaScriptDialogManager,
            public viz::mojom::FrameSinkVideoConsumer,
            public content::RenderWidgetHost::InputEventObserver,
            public TabView::Delegate {
 public:
  Tab(BrowserMainParts& parts,
      int id,
      std::string context,
      std::unique_ptr<content::WebContents> contents,
      const gfx::Size& size);
  ~Tab() override;

  int id() const { return id_; }
  const std::string& context() const { return context_; }
  content::WebContents* contents() const { return contents_.get(); }
  // Whether a viewer holds CONTROL, as of the last command the UI thread handled.
  bool controlled() const { return controlled_; }
  void set_controlled(bool controlled);
  // Whether anyone is looking, which decides how often the tab is worth capturing.
  void SetViewed(bool viewed);
  // One recording at a time; the host keeps to that.
  void StartRecording(uint64_t recording);
  void StopRecording();
  void RequestRecordingKeyFrame();

  void ReportSandbox();
  void Load(const GURL& url);
  void GoBack();
  void GoForward();
  void Reload();
  void Stop();
  void Resize(const gfx::Size& size);
  void Focus();
  void Mouse(const base::DictValue& command);
  void Wheel(const base::DictValue& command);
  void Key(const base::DictValue& command);
  void ReplyDialog(int request, bool accept, const std::u16string& text);
  void RequestFence(int token);
  void CancelFence(int token);
  void Ime(const base::DictValue& command);
  void RunClipboardCommand(const base::DictValue& command);
  // The host has given up on this command, so it must not reach the page if it hasn't.
  void ForgetClipboardCommand(int token);
  // Takes it back if it hasn't gone to its renderer, and says whether it did.
  void WithdrawClipboardCommand(int token);
  // Likewise for an IME request the host has answered for.
  void ForgetIme(int token);
  void RunMenuCommand(int command);
  void CloseContextMenu();

  // What a frame's renderer reports.
  void FocusChanged(content::RenderFrameHost& frame,
                    int counter,
                    bool editable,
                    const gfx::Rect& bounds);
  void CommandStarted(content::RenderFrameHost& frame, int token);
  void CommandFinished(content::RenderFrameHost& frame,
                       int token,
                       mojom::CommandEnd end,
                       bool selection,
                       bool editable);

  // content::WebContentsObserver:
  void DidStartLoading() override;
  void DidStopLoading() override;
  void DidFinishLoad(content::RenderFrameHost* render_frame_host,
                     const GURL& validated_url) override;
  void DidFinishNavigation(content::NavigationHandle* handle) override;
  void DidFirstVisuallyNonEmptyPaint() override;
  void RenderFrameCreated(content::RenderFrameHost* render_frame_host) override;
  void RenderFrameDeleted(content::RenderFrameHost* render_frame_host) override;
  void PrimaryPageChanged(content::Page& page) override;
  void TitleWasSet(content::NavigationEntry* entry) override;
  void PrimaryMainFrameRenderProcessGone(
      base::TerminationStatus status) override;

  // content::WebContentsDelegate:
  content::WebContents* AddNewContents(
      content::WebContents* source,
      std::unique_ptr<content::WebContents> new_contents,
      const GURL& target_url,
      WindowOpenDisposition disposition,
      const blink::mojom::WindowFeatures& window_features,
      bool user_gesture,
      bool* was_blocked) override;
  void CloseContents(content::WebContents* source) override;
  void WebContentsCreated(content::WebContents* source_contents,
                          const content::GlobalRenderFrameHostId& opener_id,
                          const std::string& frame_name,
                          const GURL& target_url,
                          content::WebContents* new_contents) override;
  bool IsWebContentsCreationOverridden(
      content::RenderFrameHost* opener,
      content::SiteInstance* source_site_instance,
      content::mojom::WindowContainerType window_container_type,
      const GURL& opener_url,
      const std::string& frame_name,
      const GURL& target_url) override;
  content::JavaScriptDialogManager* GetJavaScriptDialogManager(
      content::WebContents* source) override;
  bool HandleContextMenu(content::RenderFrameHost& render_frame_host,
                         const content::ContextMenuParams& params) override;

  // content::RenderWidgetHost::InputEventObserver:
  void OnInputEvent(const content::RenderWidgetHost& host,
                    const blink::WebInputEvent& event,
                    InputEventSource source) override;
  void OnInputEventAck(const content::RenderWidgetHost& host,
                       blink::mojom::InputEventResultSource source,
                       blink::mojom::InputEventResultState state,
                       const blink::WebInputEvent& event) override;

  // TabView::Delegate:
  void TextInputMayHaveChanged() override;
  void CaptureTargetChanged() override;
  void PictureChanged() override;

  // content::JavaScriptDialogManager:
  void RunJavaScriptDialog(content::WebContents* web_contents,
                           content::RenderFrameHost* render_frame_host,
                           content::JavaScriptDialogType dialog_type,
                           const std::u16string& message_text,
                           const std::u16string& default_prompt_text,
                           DialogClosedCallback callback,
                           bool* did_suppress_message) override;
  void RunBeforeUnloadDialog(content::WebContents* web_contents,
                             content::RenderFrameHost* render_frame_host,
                             bool is_reload,
                             DialogClosedCallback callback) override;
  void CancelDialogs(content::WebContents* web_contents,
                     bool reset_state) override;

  // viz::mojom::FrameSinkVideoConsumer:
  void OnFrameCaptured(
      media::mojom::VideoBufferHandlePtr data,
      media::mojom::VideoFrameInfoPtr info,
      const gfx::Rect& content_rect,
      mojo::PendingRemote<viz::mojom::FrameSinkVideoConsumerFrameCallbacks>
          callbacks) override;
  void OnNewCaptureVersion(
      const media::CaptureVersion& capture_version) override;
  void OnFrameWithEmptyRegionCapture() override;
  void OnStopped() override;
  void OnLog(const std::string& message) override;

 private:
  struct Dialog {
    DialogClosedCallback callback;
    // Whether it blocks the main frame's renderer, so that frame can't draw.
    bool blocks_main_frame;
  };
  // A fence waits for a forced redraw that started after it was read, then for a capture that
  // began after that redraw finished.
  struct Fence {
    int token;
    bool in_redraw = false;
    std::optional<base::TimeTicks> drawn;
  };
  class RedrawClient;
  // One thing a viewer did, in the order it did them.
  struct Ordered {
    // Runs in the renderer, whose input goes by another path: what came before must be handled.
    bool after_input;
    // Which clipboard command or IME request it is, if any.
    int clipboard_token;
    std::optional<int> ime_token;
    base::OnceClosure run;
  };
  // A copy's command that ran, waiting for what it wrote: that comes on another pipe.
  struct Ran {
    int token;
    std::string frame;
    bool selection;
    bool editable;
  };

  void Attach(const gfx::Size& size);
  void UpdateCaptureRate();
  void InOrder(bool after_input,
               int clipboard_token,
               std::optional<int> ime_token,
               base::OnceClosure run);
  void RunInOrder();
  void RendererAnswered();
  void StopWaitingForInput();
  void DispatchMouse(const base::DictValue& command);
  void DispatchWheel(const base::DictValue& command);
  void DispatchKey(const base::DictValue& command);
  void DispatchIme(const base::DictValue& command, base::TimeTicks deadline);
  void DispatchClipboardCommand(const base::DictValue& command);
  void WatchComposition();
  void OnCompositionRead(int generation,
                         const std::string& frame,
                         mojom::ImeLayoutPtr layout);
  void ImeTimedOut(int token);
  void ImeApplied(int token,
                  const std::string& frame,
                  bool applied,
                  mojom::ImeLayoutPtr layout);
  void SetLayout(content::RenderFrameHost* frame,
                 const mojom::ImeLayout& layout,
                 base::DictValue& event);
  gfx::Rect InTab(content::RenderFrameHost* frame, gfx::Rect rect);
  void WatchWidget(content::RenderWidgetHost* widget);
  void ReleaseWidget(content::RenderWidgetHost* widget);
  bool InputPending() const;
  content::RenderFrameHost* FocusedFrame();
  mojom::FrameAgent& Agent(content::RenderFrameHost& frame);
  ContextClipboard& Clipboard();
  void SendKey(ui::KeyboardCode code,
               uint32_t character,
               int flags,
               bool down);
  void RunEditShortcut(std::string command);
  void ShortcutRan(int shortcut);
  void DeliverRan();
  void ReportLoadingState();
  void SendEvent(const std::string& name, base::DictValue params);
  bool HasBlockingDialog() const;
  bool BlocksMainFrame(content::RenderFrameHost* frame) const;
  void AnswerFencesForDialog();
  void AnswerFence(int token);
  void StartRedraw();
  void OnRedraw(int id, bool drawn);
  void RefreshForFences();
  void CloseDialogs();
  void OpenFirstFrameGate();
  void PollFocusedFrame();
  void CheckFocus(content::RenderFrameHost& frame);
  void FocusChecked(content::GlobalRenderFrameHostId frame);

  const raw_ref<BrowserMainParts> parts_;
  const int id_;
  const std::string context_;
  gfx::Size size_;
  std::unique_ptr<content::WebContents> contents_;
  // Shows the contents, which go before it does (~Tab).
  std::unique_ptr<TabView> view_;
  base::ScopedClosureRunner capturer_count_;
  std::unique_ptr<viz::ClientFrameSinkVideoCapturer> capturer_;
  std::unique_ptr<Recorder> recorder_;
  // Asked for before the first paint, when it will start.
  std::optional<uint64_t> recording_waiting_;
  bool viewed_ = false;
  bool capturing_fast_ = true;
  // The capture pool's buffers, which come round again and again.
  base::flat_map<base::UnguessableToken, base::ReadOnlySharedMemoryMapping>
      mappings_;
  std::optional<uint32_t> last_counter_;
  std::map<int, Dialog> dialogs_;
  std::vector<Fence> fences_;
  base::RepeatingTimer fence_refresh_;
  base::OneShotTimer trailing_refresh_;
  std::unique_ptr<RedrawClient> redraw_client_;
  // The running redraw, which a main-frame commit replaces.
  std::optional<int> redraw_;
  int next_redraw_ = 1;
  bool renderer_gone_ = false;
  bool controlled_ = false;
  std::string last_focused_frame_;
  base::RepeatingTimer focus_poll_;
  int polls_since_check_ = 0;
  // Frames yet to answer a focus check.
  base::flat_set<content::GlobalRenderFrameHostId> checking_focus_;
  std::deque<Ordered> ordered_;
  // What's running in a renderer ahead of everything in `ordered_`. A command that has started holds
  // the renderer's main thread, so nothing that follows it can get ahead from then on.
  bool awaiting_renderer_ = false;
  int awaited_clipboard_token_ = 0;
  // While it's an IME request, whose token this is. The engine's own cancels have token zero.
  std::optional<int> awaited_ime_token_;
  // Requests the host has answered for, which their renderers may yet reply to.
  base::flat_set<int> retired_ime_;
  content::GlobalRenderFrameHostId awaited_frame_;
  base::OneShotTimer ime_wait_;
  // While it's a shortcut's command, which one; they count from one.
  std::optional<int> awaited_shortcut_;
  int shortcuts_ = 0;
  // A shortcut's key, while its command waits, and what the page made of it, once it says.
  std::optional<base::TimeTicks> shortcut_key_;
  std::optional<blink::mojom::InputEventResultState> shortcut_answer_;
  // Key and button events each widget has yet to answer. An answer for an event older than `since`
  // is for one this stopped counting, or never counted.
  struct InputWidget {
    uint64_t unanswered = 0;
    base::TimeTicks since;
  };
  std::map<const content::RenderWidgetHost*, InputWidget> input_widgets_;
  base::OneShotTimer input_wait_;
  // While composing, the page can move under it in ways nothing reports: its own layout, or a parent
  // moving an out-of-process iframe.
  base::RepeatingTimer composition_watch_;
  // A hung frame's read must not hold up, or be taken for, the current one's. Reads are of one
  // composition and one frame: either changing makes a new generation.
  int composition_generation_ = 0;
  std::optional<int> composition_read_generation_;
  std::string composition_read_frame_;
  std::optional<base::DictValue> composition_sent_;
  // Whether the page has painted, so frames reach viewers. A page with only a background never
  // reports a non-empty paint, so its load is followed by a redraw, and frames from captures that
  // began after that one count.
  bool first_paint_ = false;
  bool first_paint_redraw_requested_ = false;
  bool first_paint_redraw_running_ = false;
  std::optional<base::TimeTicks> first_paint_drawn_;
  base::OneShotTimer first_paint_timeout_;
  std::map<content::GlobalRenderFrameHostId,
           mojo::AssociatedRemote<mojom::FrameAgent>>
      agents_;
  std::optional<Ran> ran_;
  base::OneShotTimer write_wait_;
  base::WeakPtrFactory<Tab> weak_factory_{this};
};

}  // namespace lanescape_browser

#endif  // LANESCAPE_BROWSER_TAB_H_
