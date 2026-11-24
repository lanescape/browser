#include "engine/tab.h"

#include <algorithm>
#include <array>
#include <cmath>
#include <cstdint>
#include <utility>

#include "base/base64.h"
#include "base/check.h"
#include "base/no_destructor.h"
#include "base/functional/bind.h"
#include "base/json/json_reader.h"
#include "base/json/json_writer.h"
#include "base/logging.h"
#include "base/memory/read_only_shared_memory_region.h"
#include "base/process/process_handle.h"
#include "base/strings/string_number_conversions.h"
#include "base/strings/string_split.h"
#include "base/strings/stringprintf.h"
#include "base/strings/utf_string_conversions.h"
#include "base/task/single_thread_task_runner.h"
#include "base/time/time.h"
#include "base/values.h"
#include "components/viz/common/surfaces/frame_sink_id.h"
#include "components/viz/common/surfaces/video_capture_target.h"
#include "components/viz/host/host_frame_sink_manager.h"
#include "content/public/browser/context_factory.h"
#include "content/public/browser/devtools_agent_host.h"
#include "content/public/browser/devtools_agent_host_client.h"
#include "content/public/browser/navigation_controller.h"
#include "content/public/browser/navigation_details.h"
#include "content/public/browser/navigation_entry.h"
#include "content/public/browser/navigation_entry_restore_context.h"
#include "content/public/browser/navigation_handle.h"
#include "content/public/browser/page.h"
#include "content/public/browser/render_frame_host.h"
#include "content/public/browser/render_process_host.h"
#include "content/public/browser/render_widget_host_view.h"
#include "content/public/browser/restore_type.h"
#include "content/public/browser/session_storage_namespace_handle.h"
#include "services/network/public/cpp/shared_url_loader_factory.h"
#include "content/public/browser/web_contents.h"
#include "content/public/browser/context_menu_params.h"
#include "content/public/common/referrer.h"
#include "engine/browser.h"
#include "engine/clipboard.h"
#include "engine/platform.h"
#include "engine/recorder.h"
#include "media/base/video_frame.h"
#include "media/base/video_frame_metadata.h"
#include "media/base/video_types.h"
#include "media/capture/mojom/video_capture_buffer.mojom.h"
#include "media/capture/mojom/video_capture_types.mojom.h"
#include "mojo/public/cpp/bindings/callback_helpers.h"
#include "mojo/public/cpp/bindings/remote.h"
#include "third_party/blink/public/common/associated_interfaces/associated_interface_provider.h"
#include "third_party/blink/public/common/page_state/page_state.h"
#include "third_party/blink/public/common/page_state/page_state_serialization.h"
#include "third_party/blink/public/mojom/window_features/window_features.mojom.h"
#include "ui/base/ime/ime_text_span.h"
#include "ui/base/page_transition_types.h"
#include "ui/compositor/compositor.h"
#include "url/url_constants.h"
#include "ui/events/event.h"
#include "ui/events/event_constants.h"
#include "ui/events/keycodes/dom/dom_code.h"
#include "ui/events/keycodes/dom/dom_key.h"
#include "ui/events/keycodes/keyboard_code_conversion.h"
#include "ui/gfx/geometry/point_f.h"
#include "ui/gfx/geometry/quad_f.h"
#include "ui/gfx/geometry/rect.h"
#include "ui/gfx/geometry/rect_conversions.h"
#include "url/gurl.h"

namespace lanescape_browser {

namespace {

// The largest display a tab may have, as the host's own checks set it (src/lib.rs): beyond these it refuses
// CreateTab and Resize, and drops every frame.
constexpr int kMaxSide = 8192;
constexpr int64_t kMaxPixels = 16777216;

// A size a page asked for, shrunk to fit those bounds with its shape kept. Asking for nothing usable gets the
// fallback instead.
gfx::Size FitDisplay(const gfx::Size& requested, const gfx::Size& fallback) {
  const int64_t width = requested.width();
  const int64_t height = requested.height();
  if (width <= 0 || height <= 0) {
    return fallback;
  }
  if (width <= kMaxSide && height <= kMaxSide && width * height <= kMaxPixels) {
    return requested;
  }
  const double scale =
      std::min({static_cast<double>(kMaxSide) / width,
                static_cast<double>(kMaxSide) / height,
                std::sqrt(static_cast<double>(kMaxPixels) / (width * height))});
  // The small allowance stops rounding taking 5000 by 5000 to 4095 rather than 4096; the loop below still
  // keeps the result inside the bounds.
  int64_t fitted_width = std::clamp<int64_t>(
      static_cast<int64_t>(std::floor(width * scale + 1e-6)), 1, kMaxSide);
  int64_t fitted_height = std::clamp<int64_t>(
      static_cast<int64_t>(std::floor(height * scale + 1e-6)), 1, kMaxSide);
  while (fitted_width * fitted_height > kMaxPixels) {
    if (fitted_width >= fitted_height) {
      --fitted_width;
    } else {
      --fitted_height;
    }
  }
  return gfx::Size(static_cast<int>(fitted_width),
                   static_cast<int>(fitted_height));
}

// Modifier bits on the host's commands, the same values the host computes for every engine.
constexpr int kShift = 2;
constexpr int kControl = 4;
constexpr int kAlt = 8;
constexpr int kLeftButton = 16;
constexpr int kMiddleButton = 32;
constexpr int kRightButton = 64;
constexpr int kCommand = 128;

// A frame's identity for the host, which only compares them.
std::string FrameId(content::RenderFrameHost& frame) {
  const content::GlobalRenderFrameHostId id = frame.GetGlobalId();
  return base::StringPrintf("%d-%d", id.child_id.value(), id.frame_routing_id);
}

content::RenderFrameHost* FrameFromId(const std::string& id) {
  std::vector<std::string> parts = base::SplitString(
      id, "-", base::KEEP_WHITESPACE, base::SPLIT_WANT_ALL);
  int process = 0;
  int routing = 0;
  if (parts.size() != 2 || !base::StringToInt(parts[0], &process) ||
      !base::StringToInt(parts[1], &routing)) {
    return nullptr;
  }
  return content::RenderFrameHost::FromID(process, routing);
}

int EventFlags(int modifiers) {
  int flags = ui::EF_NONE;
  flags |= modifiers & kShift ? ui::EF_SHIFT_DOWN : 0;
  flags |= modifiers & kControl ? ui::EF_CONTROL_DOWN : 0;
  flags |= modifiers & kAlt ? ui::EF_ALT_DOWN : 0;
  flags |= modifiers & kCommand ? ui::EF_COMMAND_DOWN : 0;
  flags |= modifiers & kLeftButton ? ui::EF_LEFT_MOUSE_BUTTON : 0;
  flags |= modifiers & kMiddleButton ? ui::EF_MIDDLE_MOUSE_BUTTON : 0;
  flags |= modifiers & kRightButton ? ui::EF_RIGHT_MOUSE_BUTTON : 0;
  return flags;
}

}  // namespace

// The engine's own channel to a tab, separate from any client's session. Page.captureScreenshot
// makes Chromium force a redraw in the main frame's renderer and copy the surface, so its reply
// says the committed state reached the display compositor, which a renderer's own "visual state"
// answer doesn't. JPEG at the lowest quality over a tiny clip is as cheap as a capture gets, and
// the image is unused.
class Tab::RedrawClient : public content::DevToolsAgentHostClient {
 public:
  explicit RedrawClient(Tab& tab)
      : tab_(tab),
        agent_(content::DevToolsAgentHost::GetOrCreateFor(tab.contents())) {
    agent_->AttachClient(this);
  }
  ~RedrawClient() override {
    if (agent_) {
      agent_->DetachClient(this);
    }
  }

  void Redraw(int id) {
    if (!agent_) {
      tab_->OnRedraw(id, false);
      return;
    }
    std::optional<std::string> message = base::WriteJson(
        base::DictValue()
            .Set("id", id)
            .Set("method", "Page.captureScreenshot")
            .Set("params", base::DictValue()
                               .Set("format", "jpeg")
                               .Set("quality", 1)
                               .Set("optimizeForSpeed", true)));
    CHECK(message);
    agent_->DispatchProtocolMessage(this, base::as_byte_span(*message));
  }

  // content::DevToolsAgentHostClient:
  void DispatchProtocolMessage(content::DevToolsAgentHost* agent_host,
                               base::span<const uint8_t> message) override {
    std::optional<base::DictValue> reply = base::JSONReader::ReadDict(
        base::as_string_view(message), base::JSON_PARSE_CHROMIUM_EXTENSIONS);
    if (reply && reply->FindInt("id")) {
      tab_->OnRedraw(*reply->FindInt("id"), !reply->FindDict("error"));
    }
  }
  void AgentHostClosed(content::DevToolsAgentHost* agent_host) override {
    agent_ = nullptr;
  }
  bool MayReadLocalFiles() override { return false; }
  bool MayWriteLocalFiles() override { return false; }

 private:
  const raw_ref<Tab> tab_;
  scoped_refptr<content::DevToolsAgentHost> agent_;
};

Tab::Tab(BrowserMainParts& parts,
         int id,
         std::string context,
         std::unique_ptr<content::WebContents> contents,
         const gfx::Size& size)
    : parts_(parts),
      id_(id),
      context_(std::move(context)),
      size_(size),
      contents_(std::move(contents)) {
  contents_->SetDelegate(this);
  Observe(contents_.get());
  // A popup's first frame was made before this tab adopted it.
  contents_->ForEachRenderFrameHost([this](content::RenderFrameHost* frame) {
    if (frame->IsRenderFrameLive()) {
      WatchWidget(frame->GetRenderWidgetHost());
    }
  });
  Attach(size);
}

void Tab::Attach(const gfx::Size& size) {
  view_ = TabView::Create(*contents_, size, *this);

  // Keep the tab really shown: a capture count alone leaves its visibility hidden, which destroys
  // <select> popups (specs/0003-display.md composes them into the tab image).
  contents_->WasShown();
  capturer_count_ = contents_->IncrementCapturerCount(
      size, /*stay_hidden=*/false, /*stay_awake=*/false, /*is_activity=*/false);

  capturer_ =
      content::GetContextFactory()->GetHostFrameSinkManager()->CreateVideoCapturer();
  capturer_->SetFormat(media::PIXEL_FORMAT_ARGB);
  capturer_->SetResolutionConstraints(gfx::Size(1, 1), gfx::Size(8192, 8192),
                                      /*use_fixed_aspect_ratio=*/false);
  capturer_->SetAutoThrottlingEnabled(false);
  capturer_->SetAnimationFpsLockIn(false, 0);
  capturer_->SetMinCapturePeriod(base::Milliseconds(33));
  capturer_->ChangeTarget(CaptureTargetOf(view_->CaptureTarget()),
                          /*sub_capture_target_version=*/0);
  capturer_->Start(this, viz::mojom::BufferFormatPreference::kDefault);
}

Tab::~Tab() {
  ran_.reset();
  write_wait_.Stop();
  input_wait_.Stop();
  ime_wait_.Stop();
  composition_watch_.Stop();
  ordered_.clear();
  for (const auto& [widget, unanswered] : input_widgets_) {
    const_cast<content::RenderWidgetHost*>(widget)->RemoveInputEventObserver(
        this);
  }
  input_widgets_.clear();
  focus_poll_.Stop();
  first_paint_timeout_.Stop();
  fence_refresh_.Stop();
  trailing_refresh_.Stop();
  redraw_client_.reset();
  CloseDialogs();
  if (capturer_) {
    capturer_->StopAndResetConsumer();
  }
  capturer_.reset();
  StopRecording();
  capturer_count_.RunAndReset();
  view_->Detach();
  contents_->SetDelegate(nullptr);
  Observe(nullptr);
  contents_.reset();
  view_.reset();
}

void Tab::SendEvent(const std::string& name, base::DictValue params) {
  params.Set("event", name);
  params.Set("tab", id_);
  std::optional<std::string> json = base::WriteJson(params);
  CHECK(json);
  Emit(*json);
}

void Tab::ReportSandbox() {
  content::RenderProcessHost* process =
      contents_->GetPrimaryMainFrame()->GetProcess();
  const base::Process& handle = process->GetProcess();
  SendEvent("sandbox",
            RendererSandboxReport(handle.IsValid() ? handle.Pid() : 0));
}

void Tab::Load(const GURL& url) {
  contents_->GetController().LoadURL(url, content::Referrer(),
                                     ui::PAGE_TRANSITION_TYPED, std::string());
}

namespace {

void RemovePasswords(blink::ExplodedFrameState& frame) {
  if (frame.http_body.contains_passwords) {
    frame.http_body = blink::ExplodedHttpBody();
  }
  for (blink::ExplodedFrameState& child : frame.children) {
    RemovePasswords(child);
  }
}

// What a tab's snapshot may hold, so it stays well within what a later host reads back (64 MiB,
// with page state Base64-encoded). Past it, an entry is saved without its page state, and an entry
// whose addresses don't fit either isn't saved at all; the current entry always is.
constexpr size_t kSaveBudget = 40 << 20;

// One history entry as a later host restores it, within what is left of `budget`, or nothing.
std::optional<base::DictValue> SavedEntry(const content::NavigationEntry& entry,
                                          size_t& budget,
                                          bool current) {
  base::DictValue saved =
      base::DictValue()
          .Set("url", entry.GetURL().spec())
      .Set("referrer", entry.GetReferrer().url.spec())
      .Set("referrer_policy", static_cast<int>(entry.GetReferrer().policy))
      .Set("title", base::UTF16ToUTF8(entry.GetTitle()))
      .Set("transition", static_cast<int>(entry.GetTransitionType()))
      .Set("post", entry.GetHasPostData())
      .Set("original_url", entry.GetOriginalRequestURL().spec());
  // Charged at the most each can take as JSON: any character can become a six-byte escape (a
  // query keeps its backslashes, a data: URL its angle brackets). Ordinary addresses are small.
  const size_t addresses = (entry.GetURL().spec().size() +
                            entry.GetReferrer().url.spec().size() +
                            entry.GetOriginalRequestURL().spec().size() +
                            entry.GetTitle().size()) *
                               6 +
                           256;
  if (addresses > budget && !current) {
    return std::nullopt;
  }
  budget -= std::min(addresses, budget);
  if (budget == 0) {
    return saved;
  }
  // A form posted with a password keeps it in the entry, the main frame's or any iframe's; the
  // disk never gets it. (PageState::RemovePasswordData looks at the main frame alone.)
  //
  // GetPageState() encodes the entry's whole page state before its size can be checked; content's
  // API has no way to bound or estimate it. That is accepted: Chromium already holds those bytes
  // for the entry, so the copy is one entry's worth, briefly, at most once a second per tab; only
  // the page itself can make it large; and Chrome's own session restore does the same on every
  // save. The budget bounds what reaches the disk and a later host.
  const blink::PageState page_state = entry.GetPageState();
  const std::string& encoded = page_state.ToEncodedData();
  blink::ExplodedPageState exploded;
  // Saved as Base64, which is a third larger.
  const size_t cost = (encoded.size() + 2) / 3 * 4;
  if (cost <= budget && blink::DecodePageState(encoded, &exploded)) {
    budget -= cost;
    RemovePasswords(exploded.top);
    std::string without_passwords;
    blink::EncodePageState(exploded, &without_passwords);
    saved.Set("page_state", base::Base64Encode(without_passwords));
  }
  return saved;
}

// What restoring page state may cost this host, every tab together, counting the copy each frame
// gets of its subtree, which Chromium keeps for as long as the entry. UI thread only.
size_t g_restore_budget = 512 << 20;

size_t FrameDepth(const blink::ExplodedFrameState& frame) {
  size_t depth = 0;
  for (const blink::ExplodedFrameState& child : frame.children) {
    depth = std::max(depth, FrameDepth(child) + 1);
  }
  return depth;
}

}  // namespace

// What a later host needs to bring this tab back: its history, each entry's page state (form
// values, scroll position, history.state), and where it was in it. The initial empty document
// isn't history; a tab that hasn't committed anything else yet saves where it is going.
void Tab::Admit() {
  admitted_ = true;
  // Its session storage outlasts the host, as the tab does.
  contents_->GetController().GetDefaultSessionStorageNamespace()->SetShouldPersist(true);
  // A restored page runs nothing, not even a window.close(), until the host has taken it on.
  if (restored_ && restored_history_) {
    contents_->GetController().LoadIfNecessary();
  } else if (restored_) {
    Load(GURL(url::kAboutBlankURL));
  }
  Save();
}

void Tab::Save() {
  save_.Stop();
  // Only a tab the host has admitted, and so announced, may come back.
  if (!admitted_) {
    return;
  }
  content::NavigationController& controller = contents_->GetController();
  base::ListValue entries;
  int index = 0;
  size_t budget = kSaveBudget;
  for (int i = 0; i < controller.GetEntryCount(); ++i) {
    content::NavigationEntry* entry = controller.GetEntryAtIndex(i);
    if (entry->IsInitialEntry()) {
      continue;
    }
    const bool current = i == controller.GetLastCommittedEntryIndex();
    std::optional<base::DictValue> saved = SavedEntry(*entry, budget, current);
    if (!saved) {
      continue;
    }
    if (current) {
      index = static_cast<int>(entries.size());
    }
    entries.Append(std::move(*saved));
  }
  // Only while it is still on its way: one that was stopped or failed isn't where the tab is.
  if (entries.empty() && contents_->IsLoading()) {
    if (content::NavigationEntry* pending = controller.GetPendingEntry()) {
      entries.Append(*SavedEntry(*pending, budget, /*current=*/true));
    }
  }
  SendEvent("tab_state", base::DictValue()
                             .Set("context", context_)
                             .Set("session_storage",
                                  controller.GetDefaultSessionStorageNamespace()->id())
                             .Set("width", size_.width())
                             .Set("height", size_.height())
                             .Set("index", index)
                             .Set("entries", std::move(entries)));
}

void Tab::SaveSoon() {
  if (!save_.IsRunning()) {
    save_.Start(FROM_HERE, base::Seconds(1),
                base::BindOnce(&Tab::Save, base::Unretained(this)));
  }
}

void Tab::Restore(const base::DictValue& state) {
  restored_ = true;
  // Saved, so its session storage stays even if the host never admits it.
  contents_->GetController().GetDefaultSessionStorageNamespace()->SetShouldPersist(true);
  static const base::NoDestructor<base::ListValue> none;
  const base::ListValue* saved = state.FindList("entries");
  std::vector<std::unique_ptr<content::NavigationEntry>> entries;
  std::unique_ptr<content::NavigationEntryRestoreContext> restore_context =
      content::NavigationEntryRestoreContext::Create();
  for (const base::Value& value : saved ? *saved : *none) {
    const base::DictValue* saved_entry = value.GetIfDict();
    const std::string* url = saved_entry ? saved_entry->FindString("url") : nullptr;
    if (!url) {
      continue;
    }
    const std::string* referrer = saved_entry->FindString("referrer");
    const int policy = std::clamp(
        saved_entry->FindInt("referrer_policy").value_or(0), 0,
        static_cast<int>(network::mojom::ReferrerPolicy::kMaxValue));
    // A saved file is data, never trusted to hold what Chromium CHECKs: a transition it doesn't
    // know becomes a link.
    const int transition = saved_entry->FindInt("transition").value_or(0);
    std::unique_ptr<content::NavigationEntry> entry =
        content::NavigationController::CreateNavigationEntry(
            GURL(*url),
            content::Referrer(GURL(referrer ? *referrer : std::string()),
                              static_cast<network::mojom::ReferrerPolicy>(policy)),
            /*initiator_origin=*/std::nullopt,
            /*initiator_base_url=*/std::nullopt,
            ui::IsValidPageTransitionType(transition)
                ? ui::PageTransitionFromInt(transition)
                : ui::PAGE_TRANSITION_LINK,
            /*is_renderer_initiated=*/false, /*extra_headers=*/std::string(),
            contents_->GetBrowserContext(),
            /*blob_url_loader_factory=*/nullptr);
    if (const std::string* title = saved_entry->FindString("title")) {
      entry->SetTitle(base::UTF8ToUTF16(*title));
    }
    // Chromium takes the page's URL from its page state, so state that describes another page, or
    // that names files (which Chromium would let the renderer read, and which nobody here
    // authorized), or that is missing or doesn't decode, comes back as the checked URL alone.
    std::string encoded_state;
    blink::ExplodedPageState exploded;
    const bool usable =
        saved_entry->FindString("page_state") &&
        base::Base64Decode(*saved_entry->FindString("page_state"),
                           &encoded_state) &&
        blink::DecodePageState(encoded_state, &exploded) &&
        exploded.referenced_files.empty() && exploded.top.url_string &&
        GURL(*exploded.top.url_string) == GURL(*url) &&
        // Chromium gives each frame its whole subtree's state, so a nested frame's data is held
        // once per ancestor: bounded across every restored tab, or the URL alone.
        encoded_state.size() * (FrameDepth(exploded.top) + 1) <= g_restore_budget;
    if (usable) {
      g_restore_budget -= encoded_state.size() * (FrameDepth(exploded.top) + 1);
    }
    entry->SetPageState(
        usable ? blink::PageState::CreateFromEncodedData(encoded_state)
               : blink::PageState::CreateFromURL(GURL(*url)),
        restore_context.get());
    // State made from the URL alone has no referrer: the saved one still applies.
    if (!usable) {
      entry->SetReferrer(content::Referrer(
          GURL(referrer ? *referrer : std::string()),
          static_cast<network::mojom::ReferrerPolicy>(policy)));
    }
    entry->SetHasPostData(saved_entry->FindBool("post").value_or(false));
    if (const std::string* original = saved_entry->FindString("original_url")) {
      entry->SetOriginalRequestURL(GURL(*original));
    }
    entries.push_back(std::move(entry));
  }
  if (entries.empty()) {
    return;
  }
  const int index = std::clamp(state.FindInt("index").value_or(0), 0,
                               static_cast<int>(entries.size()) - 1);
  contents_->GetController().Restore(index, content::RestoreType::kRestored,
                                     &entries);
  restored_history_ = true;
}

// A popup's first navigation can take a while to commit; until then it saves where it is going.
void Tab::DidStartNavigation(content::NavigationHandle* handle) {
  if (handle->IsInPrimaryMainFrame()) {
    SaveSoon();
  }
}

void Tab::NavigationEntryCommitted(
    const content::LoadCommittedDetails& load_details) {
  SaveSoon();
}

// Where page state arrives: the renderer reports form values, scrolling and history.state here.
void Tab::NavigationEntryChanged(
    const content::EntryChangedDetails& change_details) {
  SaveSoon();
}

void Tab::NavigationListPruned(const content::PrunedDetails& pruned_details) {
  SaveSoon();
}

void Tab::NavigationEntriesDeleted() {
  SaveSoon();
}

void Tab::GoBack() {
  if (contents_->GetController().CanGoBack()) {
    contents_->GetController().GoBack();
  }
}

void Tab::GoForward() {
  if (contents_->GetController().CanGoForward()) {
    contents_->GetController().GoForward();
  }
}

void Tab::Reload() {
  contents_->GetController().Reload(content::ReloadType::NORMAL,
                                    /*check_for_repost=*/false);
}

void Tab::Stop() {
  contents_->Stop();
}

void Tab::Resize(const gfx::Size& size) {
  size_ = size;
  SaveSoon();
  // The capture pool is about to be replaced by one of the new size.
  mappings_.clear();
  view_->Resize(size);
}

void Tab::Focus() {
  contents_->Focus();
}

// Input reaches a renderer by a path of its own, which messages to a frame can overtake either way,
// so a viewer's typing and its IME or clipboard commands must not overlap. A command waits for the
// input before it to be handled, and whatever follows waits for the command to start.
void Tab::InOrder(bool after_input,
                  int clipboard_token,
                  std::optional<int> ime_token,
                  base::OnceClosure run) {
  ordered_.push_back({after_input, clipboard_token, ime_token, std::move(run)});
  RunInOrder();
}

void Tab::RunInOrder() {
  while (!ordered_.empty() && !awaiting_renderer_) {
    if (ordered_.front().after_input && InputPending()) {
      // A renderer that never answers can't hold up everything forever.
      if (!input_wait_.IsRunning()) {
        input_wait_.Start(FROM_HERE, base::Seconds(2), this,
                          &Tab::StopWaitingForInput);
      }
      return;
    }
    input_wait_.Stop();
    base::OnceClosure run = std::move(ordered_.front().run);
    ordered_.pop_front();
    std::move(run).Run();
  }
}

bool Tab::InputPending() const {
  for (const auto& [widget, input] : input_widgets_) {
    if (input.unanswered > 0) {
      return true;
    }
  }
  return false;
}

void Tab::RendererAnswered() {
  ime_wait_.Stop();
  awaiting_renderer_ = false;
  awaited_clipboard_token_ = 0;
  awaited_ime_token_.reset();
  awaited_shortcut_.reset();
  awaited_frame_ = content::GlobalRenderFrameHostId();
  RunInOrder();
}

// What's outstanding stops counting, and its answers, should they come, are recognized by their age.
void Tab::StopWaitingForInput() {
  const base::TimeTicks now = base::TimeTicks::Now();
  for (auto& [widget, input] : input_widgets_) {
    input.unanswered = 0;
    input.since = now;
  }
  RunInOrder();
}

namespace {
// What a viewer's order depends on. Pointer moves aren't: they carry no text, and Chromium coalesces
// them, so they don't each get an answer.
bool IsOrderedInput(const blink::WebInputEvent& event) {
  const blink::WebInputEvent::Type type = event.GetType();
  return blink::WebInputEvent::IsKeyboardEventType(type) ||
         type == blink::WebInputEvent::Type::kMouseDown ||
         type == blink::WebInputEvent::Type::kMouseUp;
}
}  // namespace

void Tab::OnInputEvent(const content::RenderWidgetHost& host,
                       const blink::WebInputEvent& event,
                       InputEventSource source) {
  auto widget = input_widgets_.find(&host);
  if (IsOrderedInput(event) && widget != input_widgets_.end() &&
      event.TimeStamp() >= widget->second.since) {
    ++widget->second.unanswered;
  }
}

void Tab::OnInputEventAck(const content::RenderWidgetHost& host,
                          blink::mojom::InputEventResultSource source,
                          blink::mojom::InputEventResultState state,
                          const blink::WebInputEvent& event) {
  // Whether the page took a shortcut's key, for the shortcut waiting on it (RunEditShortcut).
  if (event.GetType() == blink::WebInputEvent::Type::kRawKeyDown &&
      shortcut_key_ == event.TimeStamp()) {
    shortcut_answer_ = state;
  }
  auto widget = input_widgets_.find(&host);
  if (IsOrderedInput(event) && widget != input_widgets_.end() &&
      widget->second.unanswered > 0 &&
      event.TimeStamp() >= widget->second.since) {
    --widget->second.unanswered;
    RunInOrder();
  }
}

void Tab::RenderFrameCreated(content::RenderFrameHost* render_frame_host) {
  WatchWidget(render_frame_host->GetRenderWidgetHost());
}

void Tab::WatchWidget(content::RenderWidgetHost* widget) {
  auto [it, added] = input_widgets_.try_emplace(widget);
  if (added) {
    it->second.since = base::TimeTicks::Now();
    widget->AddInputEventObserver(this);
  }
}

// A widget going away never answers.
void Tab::ReleaseWidget(content::RenderWidgetHost* widget) {
  auto it = input_widgets_.find(widget);
  if (it != input_widgets_.end()) {
    widget->RemoveInputEventObserver(this);
    input_widgets_.erase(it);
  }
}

void Tab::Mouse(const base::DictValue& command) {
  InOrder(false, 0, std::nullopt,
          base::BindOnce(&Tab::DispatchMouse, weak_factory_.GetWeakPtr(),
                         command.Clone()));
}

void Tab::Wheel(const base::DictValue& command) {
  InOrder(false, 0, std::nullopt,
          base::BindOnce(&Tab::DispatchWheel, weak_factory_.GetWeakPtr(),
                         command.Clone()));
}

void Tab::Key(const base::DictValue& command) {
  InOrder(false, 0, std::nullopt,
          base::BindOnce(&Tab::DispatchKey, weak_factory_.GetWeakPtr(),
                         command.Clone()));
}

void Tab::DispatchMouse(const base::DictValue& command) {
  const gfx::Point at(command.FindInt("x").value_or(0),
                      command.FindInt("y").value_or(0));
  const std::string* kind = command.FindString("kind");
  CHECK(kind);
  const int flags = EventFlags(command.FindInt("modifiers").value_or(0));
  const int button = command.FindInt("button").value_or(0);
  const int changed = button == 1   ? ui::EF_LEFT_MOUSE_BUTTON
                      : button == 2 ? ui::EF_MIDDLE_MOUSE_BUTTON
                      : button == 4 ? ui::EF_RIGHT_MOUSE_BUTTON
                                    : 0;
  ui::EventType type = ui::EventType::kMouseMoved;
  if (*kind == "down") {
    type = ui::EventType::kMousePressed;
  } else if (*kind == "up") {
    type = ui::EventType::kMouseReleased;
  } else if (flags & (ui::EF_LEFT_MOUSE_BUTTON | ui::EF_MIDDLE_MOUSE_BUTTON |
                      ui::EF_RIGHT_MOUSE_BUTTON)) {
    type = ui::EventType::kMouseDragged;
  }
  ui::MouseEvent event(type, at, at, base::TimeTicks::Now(), flags | changed,
                       changed);
  if (const std::optional<int> clicks = command.FindInt("clicks")) {
    event.SetClickCount(*clicks);
  }
  view_->Mouse(event);
}

void Tab::DispatchWheel(const base::DictValue& command) {
  const gfx::Point at(command.FindInt("x").value_or(0),
                      command.FindInt("y").value_or(0));
  ui::MouseWheelEvent event(gfx::Vector2d(command.FindInt("dx").value_or(0),
                                          command.FindInt("dy").value_or(0)),
                            at, at, base::TimeTicks::Now(),
                            EventFlags(command.FindInt("modifiers").value_or(0)),
                            0);
  view_->Wheel(event);
}

void Tab::DispatchKey(const base::DictValue& command) {
  const auto code = static_cast<ui::KeyboardCode>(command.FindInt("vk").value_or(0));
  const uint32_t character =
      static_cast<uint32_t>(command.FindInt("character").value_or(0));
  const int flags = EventFlags(command.FindInt("modifiers").value_or(0));
  SendKey(code, character, flags, command.FindBool("down").value_or(false));
}

void Tab::SendKey(ui::KeyboardCode code,
                  uint32_t character,
                  int flags,
                  bool down) {
  if (code == ui::VKEY_UNKNOWN) {
    // A Unicode keysym with no key behind it: it can only be typed.
    if (!down || !character) {
      return;
    }
    view_->Type(static_cast<char16_t>(character), flags);
    return;
  }
  const ui::DomCode dom_code = ui::UsLayoutKeyboardCodeToDomCode(code);
  ui::DomKey dom_key;
  ui::KeyboardCode unused_code;
  if (character >= 0x20) {
    dom_key = ui::DomKey::FromCharacter(character);
  } else if (!ui::DomCodeToUsLayoutDomKey(dom_code, flags, &dom_key, &unused_code)) {
    dom_key = ui::DomKey::UNIDENTIFIED;
  }
  ui::KeyEvent event(
      down ? ui::EventType::kKeyPressed : ui::EventType::kKeyReleased, code,
      dom_code, flags, dom_key, base::TimeTicks::Now());
  view_->Key(event);
  // A shortcut runs once the page has had the key and left it alone, and before anything after it.
  if (const std::optional<std::string_view> command =
          view_->EditShortcut(event)) {
    shortcut_key_ = event.time_stamp();
    shortcut_answer_.reset();
    ordered_.push_front(
        {true, 0, std::nullopt,
         base::BindOnce(&Tab::RunEditShortcut, weak_factory_.GetWeakPtr(),
                        std::string(*command))});
  }
}

void Tab::RunEditShortcut(std::string command) {
  // Only once the page has said it left this key alone. A renderer too busy to say within the
  // input's bound never gets the command: it may yet have taken the key.
  const bool unhandled = shortcut_answer_ ==
                         blink::mojom::InputEventResultState::kNotConsumed;
  shortcut_key_.reset();
  shortcut_answer_.reset();
  content::RenderFrameHost* frame = FocusedFrame();
  if (!unhandled || !frame->IsRenderFrameLive()) {
    return;
  }
  // Run by the frame's agent, as its IME and clipboard commands are, so that it is ordered with them,
  // and later input waits for its answer, or for a hung renderer's bound.
  // A renderer that gets to it after the bound, when later input has gone, refuses it.
  const base::TimeTicks deadline = base::TimeTicks::Now() + base::Seconds(2);
  // Each its own, so that an earlier one's late answer can't stand in for it.
  const int shortcut = ++shortcuts_;
  awaiting_renderer_ = true;
  awaited_frame_ = frame->GetGlobalId();
  awaited_shortcut_ = shortcut;
  ime_wait_.Start(FROM_HERE, deadline - base::TimeTicks::Now(),
                  base::BindOnce(&Tab::ShortcutRan, weak_factory_.GetWeakPtr(),
                                 shortcut));
  Agent(*frame).RunEditCommand(
      command, deadline,
      mojo::WrapCallbackWithDefaultInvokeIfNotRun(base::BindOnce(
          &Tab::ShortcutRan, weak_factory_.GetWeakPtr(), shortcut)));
}

void Tab::ShortcutRan(int shortcut) {
  if (awaiting_renderer_ && awaited_shortcut_ == shortcut) {
    RendererAnswered();
  }
}

void Tab::ReplyDialog(int request, bool accept, const std::u16string& text) {
  auto it = dialogs_.find(request);
  if (it == dialogs_.end()) {
    return;
  }
  DialogClosedCallback callback = std::move(it->second.callback);
  dialogs_.erase(it);
  std::move(callback).Run(accept, text);
}

bool Tab::HasBlockingDialog() const {
  for (const auto& [request, dialog] : dialogs_) {
    if (dialog.blocks_main_frame) {
      return true;
    }
  }
  return false;
}

void Tab::AnswerFence(int token) {
  SendEvent("fence_ready", base::DictValue().Set("token", token));
}

void Tab::RequestFence(int token) {
  if (renderer_gone_) {
    SendEvent("fence_failed", base::DictValue()
                                  .Set("token", token)
                                  .Set("reason", "display fence failed"));
    return;
  }
  // A renderer blocked by a dialog can't draw: the latest frame is what a person at the page sees.
  if (HasBlockingDialog() && last_counter_) {
    AnswerFence(token);
    return;
  }
  fences_.push_back({token});
  UpdateCaptureRate();
  StartRedraw();
  if (!fence_refresh_.IsRunning()) {
    fence_refresh_.Start(FROM_HERE, base::Milliseconds(100), this,
                         &Tab::RefreshForFences);
  }
}

void Tab::CancelFence(int token) {
  std::erase_if(fences_, [&](const Fence& f) { return f.token == token; });
  if (fences_.empty()) {
    fence_refresh_.Stop();
  }
  UpdateCaptureRate();
}

// Chromium can't cancel a redraw, so a tab runs one at a time, and every fence read before it
// starts shares it. A tab with no frame yet has nothing to redraw.
void Tab::StartRedraw() {
  if (redraw_ || renderer_gone_) {
    return;
  }
  bool waiting = false;
  if (first_paint_redraw_requested_ && !first_paint_drawn_ &&
      !first_paint_redraw_running_) {
    first_paint_redraw_running_ = true;
    waiting = true;
  }
  // A tab with no frame has nothing a fence's answer could name, so a fence waits for the first.
  if (last_counter_) {
    for (Fence& fence : fences_) {
      if (!fence.in_redraw && !fence.drawn) {
        fence.in_redraw = true;
        waiting = true;
      }
    }
  }
  if (!waiting) {
    return;
  }
  if (!redraw_client_) {
    redraw_client_ = std::make_unique<RedrawClient>(*this);
  }
  redraw_ = next_redraw_++;
  redraw_client_->Redraw(*redraw_);
}

void Tab::OnRedraw(int id, bool drawn) {
  if (redraw_ != id) {
    return;  // A commit replaced it.
  }
  redraw_.reset();
  const base::TimeTicks now = base::TimeTicks::Now();
  if (first_paint_redraw_running_) {
    first_paint_redraw_running_ = false;
    if (drawn) {
      first_paint_drawn_ = now;
    } else if (!first_paint_ && !fence_refresh_.IsRunning()) {
      fence_refresh_.Start(FROM_HERE, base::Milliseconds(100), this,
                           &Tab::RefreshForFences);
    }
  }
  for (Fence& fence : fences_) {
    if (fence.in_redraw) {
      fence.in_redraw = false;
      if (drawn) {
        fence.drawn = now;
      }
    }
  }
  if (drawn) {
    capturer_->RequestRefreshFrame();
  }
  StartRedraw();
}

// A refresh can be skipped while another capture runs, and a redraw that failed runs again, so
// both repeat until the host's bound ends the fence.
void Tab::RefreshForFences() {
  bool waiting_for_capture =
      !last_counter_ && (first_paint_ || first_paint_drawn_.has_value());
  for (const Fence& fence : fences_) {
    waiting_for_capture |= fence.drawn.has_value();
  }
  if (waiting_for_capture) {
    capturer_->RequestRefreshFrame();
  }
  StartRedraw();
}

void Tab::StartRecording(uint64_t recording) {
  if (first_paint_) {
    recorder_ =
        std::make_unique<Recorder>(*view_, recording);
  } else {
    recording_waiting_ = recording;
  }
}

void Tab::StopRecording() {
  if (recording_waiting_) {
    DeliverRecordingEnd(*recording_waiting_);
    recording_waiting_.reset();
  }
  recorder_.reset();
}

void Tab::RequestRecordingKeyFrame() {
  if (recorder_) {
    recorder_->RequestKeyFrame();
  }
}

void Tab::SetViewed(bool viewed) {
  viewed_ = viewed;
  UpdateCaptureRate();
}

// Capturing at full rate for nobody is wasted. A slow capture keeps the latest frame recent, for a
// viewer connecting. A fence and the first frame wait on a capture, so they count as demand.
void Tab::UpdateCaptureRate() {
  const bool fast = viewed_ || !fences_.empty() || !last_counter_;
  if (fast == capturing_fast_) {
    return;
  }
  capturing_fast_ = fast;
  capturer_->SetMinCapturePeriod(fast ? base::Milliseconds(33) : base::Seconds(1));
  if (fast) {
    capturer_->RequestRefreshFrame();
  }
}

void Tab::set_controlled(bool controlled) {
  controlled_ = controlled;
  if (controlled && !focus_poll_.IsRunning()) {
    focus_poll_.Start(FROM_HERE, base::Milliseconds(25), this,
                      &Tab::PollFocusedFrame);
    PollFocusedFrame();
  } else if (!controlled) {
    focus_poll_.Stop();
  }
}

// The browser's focused frame: an iframe holding focus, otherwise the main frame.
content::RenderFrameHost* Tab::FocusedFrame() {
  content::RenderFrameHost* focused = contents_->GetFocusedFrame();
  return focused ? focused : contents_->GetPrimaryMainFrame();
}

void Tab::PollFocusedFrame() {
  std::string id = FrameId(*FocusedFrame());
  if (id != last_focused_frame_) {
    last_focused_frame_ = id;
    SendEvent("frame_focused", base::DictValue().Set("frame", id));
  }
  // Chromium's text input state says when a frame starts or stops taking text, but only if it differs
  // from what it last said, and a page can change it and back between updates. This catches the rest.
  // Acting on a stale answer is safe either way: a request is checked again as it's applied.
  constexpr int kPollsPerCheck = 8;
  if (++polls_since_check_ >= kPollsPerCheck) {
    polls_since_check_ = 0;
    CheckFocus(*FocusedFrame());
  }
}

void Tab::CheckFocus(content::RenderFrameHost& frame) {
  if (!frame.IsRenderFrameLive()) {
    return;
  }
  // One at a time, so that a renderer running a long script doesn't come back to a pile of them.
  const content::GlobalRenderFrameHostId id = frame.GetGlobalId();
  if (!checking_focus_.insert(id).second) {
    return;
  }
  Agent(frame).CheckFocus(mojo::WrapCallbackWithDefaultInvokeIfNotRun(
      base::BindOnce(&Tab::FocusChecked, weak_factory_.GetWeakPtr(), id)));
}

void Tab::FocusChecked(content::GlobalRenderFrameHostId frame) {
  checking_focus_.erase(frame);
}

void Tab::FocusChanged(content::RenderFrameHost& frame,
                       int counter,
                       bool editable,
                       const gfx::Rect& bounds) {
  // The renderer counts from one, and anything else isn't worth believing.
  if (counter <= 0) {
    return;
  }
  const gfx::Rect in_tab = InTab(&frame, bounds);
  SendEvent("focus_report",
            base::DictValue()
                .Set("frame", FrameId(frame))
                .Set("focused_frame", FrameId(*FocusedFrame()))
                .Set("counter", counter)
                .Set("editable", editable)
                .Set("x", in_tab.x())
                .Set("y", in_tab.y())
                .Set("width", in_tab.width())
                .Set("height", in_tab.height()));
}

void Tab::TextInputMayHaveChanged() {
  CheckFocus(*FocusedFrame());
}

// A navigation swapped the page's view, on a system where that is what's captured.
void Tab::PictureChanged() {
  capturer_->RequestRefreshFrame();
}

void Tab::CaptureTargetChanged() {
  capturer_->ChangeTarget(CaptureTargetOf(view_->CaptureTarget()),
                          /*sub_capture_target_version=*/0);
  capturer_->RequestRefreshFrame();
  if (recorder_) {
    recorder_->ChangeTarget(view_->CaptureTarget());
  }
}

// The renderer knows only its own widget, which for an out-of-process iframe isn't the tab's.
gfx::Rect Tab::InTab(content::RenderFrameHost* frame, gfx::Rect rect) {
  content::RenderWidgetHostView* view = frame ? frame->GetView() : nullptr;
  content::RenderWidgetHostView* root = contents_->GetRenderWidgetHostView();
  if (!view || view == root) {
    return rect;
  }
  // This knows about CSS transforms, but needs the iframe to have been drawn, and until then leaves
  // every point where it was. For an iframe away from the tab's corner that can't be its real
  // transform, and the view's own position holds from the start. Only nested iframes whose rotations
  // exactly cancel could look the same, and Chromium says which it was only behind its internal API.
  const gfx::Vector2d offset =
      root ? view->GetViewBounds().origin() - root->GetViewBounds().origin()
           : gfx::Vector2d();
  // Three points in no line: only the identity leaves them all where they were.
  const bool unavailable =
      !offset.IsZero() &&
      std::ranges::all_of(
          std::array{gfx::Point(0, 0), gfx::Point(1000, 0), gfx::Point(0, 1000)},
          [view](const gfx::Point& probe) {
            return view->TransformPointToRootCoordSpace(probe) == probe;
          });
  if (unavailable) {
    return rect + offset;
  }
  // All four corners, as a transform can scale or rotate the rectangle as well as move it. Rounded
  // outwards, so that a caret scaled down stays visible, and past the error in the transform's floats.
  const auto transformed = [view](const gfx::Point& corner) {
    return view->TransformPointToRootCoordSpaceF(gfx::PointF(corner));
  };
  return gfx::ToEnclosingRectIgnoringError(
      gfx::QuadF(transformed(rect.origin()), transformed(rect.top_right()),
                 transformed(rect.bottom_right()),
                 transformed(rect.bottom_left()))
          .BoundingBox(),
      0.001f);
}

void Tab::SetLayout(content::RenderFrameHost* frame,
                    const mojom::ImeLayout& layout,
                    base::DictValue& event) {
  auto in_tab = [this, frame](gfx::Rect rect) {
    rect = InTab(frame, rect);
    return base::DictValue()
        .Set("x", rect.x())
        .Set("y", rect.y())
        .Set("width", rect.width())
        .Set("height", rect.height());
  };
  base::ListValue bounds;
  for (const gfx::Rect& rect : layout.character_bounds) {
    bounds.Append(in_tab(rect));
  }
  event.Set("composing", layout.composing);
  event.Set("caret", in_tab(layout.caret));
  event.Set("bounds", std::move(bounds));
}

void Tab::RenderFrameDeleted(content::RenderFrameHost* render_frame_host) {
  const content::GlobalRenderFrameHostId id = render_frame_host->GetGlobalId();
  content::RenderWidgetHost* widget = render_frame_host->GetRenderWidgetHost();
  content::RenderFrameHost* parent = render_frame_host->GetParent();
  if (!parent || parent->GetRenderWidgetHost() != widget) {
    ReleaseWidget(widget);
  }
  // Dropping its agent answers an IME request still there.
  agents_.erase(id);
  if (awaiting_renderer_ && awaited_frame_ == id) {
    RendererAnswered();
  } else {
    RunInOrder();
  }
  SendEvent("frame_gone",
            base::DictValue().Set("frame", FrameId(*render_frame_host)));
}

mojom::FrameAgent& Tab::Agent(content::RenderFrameHost& frame) {
  mojo::AssociatedRemote<mojom::FrameAgent>& agent =
      agents_[frame.GetGlobalId()];
  if (!agent) {
    frame.GetRemoteAssociatedInterfaces()->GetInterface(&agent);
  }
  return *agent;
}

ContextClipboard& Tab::Clipboard() {
  return static_cast<BrowserContext*>(contents_->GetBrowserContext())
      ->clipboard();
}

void Tab::RunClipboardCommand(const base::DictValue& command) {
  InOrder(true, command.FindInt("token").value_or(0), std::nullopt,
          base::BindOnce(&Tab::DispatchClipboardCommand,
                         weak_factory_.GetWeakPtr(), command.Clone()));
}

void Tab::WithdrawClipboardCommand(int token) {
  const size_t withdrawn = std::erase_if(
      ordered_, [&](const Ordered& o) { return o.clipboard_token == token; });
  SendEvent("clipboard_withdrawn", base::DictValue()
                                       .Set("token", token)
                                       .Set("withdrawn", withdrawn > 0));
  RunInOrder();
}

void Tab::ForgetClipboardCommand(int token) {
  std::erase_if(ordered_,
                [&](const Ordered& o) { return o.clipboard_token == token; });
  if (awaiting_renderer_ && awaited_clipboard_token_ == token) {
    RendererAnswered();
  } else {
    RunInOrder();
  }
}

void Tab::DispatchClipboardCommand(const base::DictValue& command) {
  const int token = command.FindInt("token").value_or(0);
  const int kind = command.FindInt("kind").value_or(0);
  const std::string* text = command.FindString("text");
  const std::string* editor = command.FindString("frame");
  content::RenderFrameHost* frame = FocusedFrame();
  const std::string id = FrameId(*frame);
  const bool copies = kind == static_cast<int>(mojom::ClipboardCommand::kCopy) ||
                      kind == static_cast<int>(mojom::ClipboardCommand::kCut);
  // An upload belongs to the editor it was made for. Focus can have left that frame since the host
  // last heard, and the report on its way only ends the operation once this has run.
  if (!frame->IsRenderFrameLive() || (!copies && (!editor || *editor != id))) {
    SendEvent("clipboard_ran", base::DictValue()
                                   .Set("token", token)
                                   .Set("frame", id)
                                   .Set("stage", 2));
    return;
  }
  ran_.reset();
  write_wait_.Stop();
  Clipboard().Begin(token, frame->GetGlobalId(), copies,
                    text ? base::UTF8ToUTF16(*text) : std::u16string());
  SendEvent("clipboard_sent",
            base::DictValue().Set("token", token).Set("frame", id));
  // The focus it was sent for, when this frame holds that focus.
  const int counter =
      editor && *editor == id ? command.FindInt("counter").value_or(0) : 0;
  awaiting_renderer_ = true;
  awaited_clipboard_token_ = token;
  awaited_frame_ = frame->GetGlobalId();
  // A busy renderer that starts it later refuses it, and the host decides what then.
  Agent(*frame).RunCommand(token, static_cast<mojom::ClipboardCommand>(kind),
                           base::TimeTicks::Now() + base::Milliseconds(500),
                           counter);
}

void Tab::CommandStarted(content::RenderFrameHost& frame, int token) {
  if (awaited_clipboard_token_ == token) {
    RendererAnswered();
  }
  if (Clipboard().RunsIn(token, frame.GetGlobalId())) {
    SendEvent("clipboard_ran", base::DictValue()
                                   .Set("token", token)
                                   .Set("frame", FrameId(frame))
                                   .Set("stage", 0));
  }
}

void Tab::CommandFinished(content::RenderFrameHost& frame,
                          int token,
                          mojom::CommandEnd end,
                          bool selection,
                          bool editable) {
  if (awaited_clipboard_token_ == token) {
    RendererAnswered();
  }
  ContextClipboard& clipboard = Clipboard();
  if (!clipboard.RunsIn(token, frame.GetGlobalId())) {
    return;
  }
  if (end != mojom::CommandEnd::kRan) {
    SendEvent("clipboard_ran",
              base::DictValue()
                  .Set("token", token)
                  .Set("frame", FrameId(frame))
                  .Set("stage", end == mojom::CommandEnd::kTooLate ? 3 : 2));
    clipboard.End(token);
    return;
  }
  ran_ = Ran{token, FrameId(frame), selection, editable};
  // What it wrote comes on the page's clipboard pipe, which this overtakes in about half of copies,
  // by under a millisecond in 350 with the CPU overloaded twice over. Blink holds that pipe's other
  // end, so no barrier can be put on it. A command that wrote nothing, because the page cancelled
  // it or nothing was selected, therefore waits out the bound. A write later still would be
  // reported as nothing copied, so the bound is generous: it costs only those commands' answers.
  if (clipboard.copies() && !clipboard.committed()) {
    clipboard.WhenCommitted(base::BindOnce(&Tab::DeliverRan,
                                           weak_factory_.GetWeakPtr()));
    write_wait_.Start(FROM_HERE, base::Milliseconds(500), this,
                      &Tab::DeliverRan);
    return;
  }
  DeliverRan();
}

void Tab::DeliverRan() {
  if (!ran_) {
    return;
  }
  const Ran ran = *std::exchange(ran_, std::nullopt);
  write_wait_.Stop();
  ContextClipboard& clipboard = Clipboard();
  if (!clipboard.Runs(ran.token)) {
    return;
  }
  base::DictValue event = base::DictValue()
                              .Set("token", ran.token)
                              .Set("frame", ran.frame)
                              .Set("stage", 1)
                              .Set("selection", ran.selection)
                              .Set("editable", ran.editable);
  if (const std::optional<ContextClipboard::Written>& written =
          clipboard.written()) {
    std::string text = base::UTF16ToUTF8(written->text);
    const bool too_large =
        written->too_large || text.size() > ContextClipboard::kTextLimit;
    if (too_large) {
      text.clear();
    }
    event.Set("nonempty", written->other || too_large || !text.empty());
    event.Set("too_large", too_large);
    event.Set("text", std::move(text));
  }
  SendEvent("clipboard_ran", std::move(event));
  clipboard.End(ran.token);
}

// The menu's own commands other than the clipboard's, on the focused frame or the tab.
void Tab::RunMenuCommand(int command) {
  switch (command) {
    case 100:
      GoBack();
      break;
    case 101:
      GoForward();
      break;
    case 110:
      contents_->Undo();
      break;
    case 111:
      contents_->Redo();
      break;
    case 116:
      contents_->Delete();
      break;
    case 117:
      contents_->SelectAll();
      break;
    default:
      break;
  }
}

void Tab::CloseContextMenu() {
  contents_->NotifyContextMenuClosed(GURL());
}

bool Tab::HandleContextMenu(content::RenderFrameHost& render_frame_host,
                            const content::ContextMenuParams& params) {
  content::NavigationController& controller = contents_->GetController();
  SendEvent("context_menu",
            base::DictValue()
                .Set("x", params.x)
                .Set("y", params.y)
                .Set("editable", params.is_editable)
                .Set("flags", params.edit_flags)
                .Set("selection", !params.selection_text.empty())
                .Set("back", controller.CanGoBack())
                .Set("forward", controller.CanGoForward()));
  return true;
}

void Tab::Ime(const base::DictValue& command) {
  // The engine's own cancels name no frame, concern whatever is composing, and need no order. They
  // never expire: a composition left behind would stay in the page.
  const bool viewer = command.FindString("frame") != nullptr;
  const std::optional<double> deadline = command.FindDouble("deadline_us");
  InOrder(viewer, 0,
          viewer ? std::optional<int>(command.FindInt("token").value_or(0))
                 : std::nullopt,
          base::BindOnce(&Tab::DispatchIme, weak_factory_.GetWeakPtr(),
                         command.Clone(),
                         deadline ? base::TimeTicks() +
                                        base::Microseconds(*deadline)
                                  : base::TimeTicks::Max()));
}

void Tab::ForgetIme(int token) {
  const size_t waiting = std::erase_if(
      ordered_, [&](const Ordered& o) { return o.ime_token == token; });
  // Gone to its renderer, or back from it already: tokens only go up, so old ones can be dropped.
  if (!waiting) {
    retired_ime_.insert(token);
    if (retired_ime_.size() > 32) {
      retired_ime_.erase(retired_ime_.begin());
    }
  }
  if (awaiting_renderer_ && awaited_ime_token_ == token) {
    RendererAnswered();
  } else {
    RunInOrder();
  }
}

void Tab::DispatchIme(const base::DictValue& command, base::TimeTicks deadline) {
  const std::string* kind = command.FindString("kind");
  CHECK(kind);
  const int token = command.FindInt("token").value_or(0);
  const std::string* editor = command.FindString("frame");
  content::RenderFrameHost* frame = FocusedFrame();
  const std::string id = FrameId(*frame);
  // A viewer's request was checked against its editor's frame by the host, on another thread.
  if (!frame->IsRenderFrameLive() || (editor && *editor != id) ||
      base::TimeTicks::Now() > deadline) {
    ImeApplied(token, id, false, mojom::ImeLayout::New());
    return;
  }
  const mojom::ImeAction action =
      *kind == "preedit"  ? mojom::ImeAction::kPreedit
      : *kind == "commit" ? mojom::ImeAction::kCommit
      : *kind == "finish" ? mojom::ImeAction::kFinish
                          : mojom::ImeAction::kCancel;
  const std::string* text = command.FindString("text");
  std::vector<ui::ImeTextSpan> spans;
  if (const base::ListValue* underlines = command.FindList("underlines")) {
    for (const base::Value& value : *underlines) {
      const base::DictValue& u = value.GetDict();
      constexpr ui::ImeTextSpan::UnderlineStyle kStyles[] = {
          ui::ImeTextSpan::UnderlineStyle::kSolid,
          ui::ImeTextSpan::UnderlineStyle::kDot,
          ui::ImeTextSpan::UnderlineStyle::kDash,
          ui::ImeTextSpan::UnderlineStyle::kNone,
      };
      const int style = u.FindInt("style").value_or(0);
      ui::ImeTextSpan span(
          ui::ImeTextSpan::Type::kComposition, u.FindInt("start").value_or(0),
          u.FindInt("end").value_or(0),
          u.FindBool("thick").value_or(false)
              ? ui::ImeTextSpan::Thickness::kThick
              : ui::ImeTextSpan::Thickness::kThin,
          UNSAFE_TODO(kStyles[style]),
          static_cast<SkColor>(u.FindDouble("background").value_or(0)));
      span.underline_color =
          static_cast<SkColor>(u.FindDouble("color").value_or(0));
      spans.push_back(span);
    }
  }
  if (editor) {
    awaiting_renderer_ = true;
    awaited_ime_token_ = token;
    awaited_frame_ = frame->GetGlobalId();
    // A renderer that never gets to it can't hold up what follows forever. The host's own bound is
    // longer, and one that does get to it too late refuses it.
    ime_wait_.Start(FROM_HERE, deadline - base::TimeTicks::Now(),
                    base::BindOnce(&Tab::ImeTimedOut,
                                   weak_factory_.GetWeakPtr(), token));
  }
  // Answered even when the frame goes away first.
  Agent(*frame).ApplyIme(
      command.FindInt("counter").value_or(0),
      deadline, action,
      text ? base::UTF8ToUTF16(*text) : std::u16string(),
      command.FindInt("start").value_or(0), command.FindInt("end").value_or(0),
      spans,
      mojo::WrapCallbackWithDefaultInvokeIfNotRun(
          base::BindOnce(&Tab::ImeApplied, weak_factory_.GetWeakPtr(), token, id),
          false, mojom::ImeLayout::New()));
}

// A request's answer and what's composing are separate things. Only a request that ran, in the frame
// that still has focus, says what's composing now. The host may have answered for it already, when
// it needs no answer but may well have started a composition that has to be cleaned up later.
void Tab::ImeApplied(int token,
                     const std::string& frame,
                     bool applied,
                     mojom::ImeLayoutPtr layout) {
  const bool retired = retired_ime_.erase(token);
  const bool observed = applied && frame == FrameId(*FocusedFrame());
  const bool composing = layout->composing;
  base::DictValue event;
  SetLayout(FrameFromId(frame), *layout, event);
  if (observed) {
    composition_sent_ = event.Clone();
    ++composition_generation_;
    if (composing && !composition_watch_.IsRunning()) {
      composition_watch_.Start(FROM_HERE, base::Milliseconds(100), this,
                               &Tab::WatchComposition);
    } else if (!composing) {
      composition_watch_.Stop();
    }
  }
  if (!retired) {
    event.Set("token", token);
    event.Set("ok", applied);
    event.Set("observed", observed);
    SendEvent("ime_applied", std::move(event));
  } else if (observed) {
    SendEvent("composition_layout", std::move(event));
  }
  if (awaiting_renderer_ && awaited_ime_token_ == token) {
    RendererAnswered();
  }
}

void Tab::ImeTimedOut(int token) {
  if (awaiting_renderer_ && awaited_ime_token_ == token) {
    RendererAnswered();
  }
}

void Tab::WatchComposition() {
  content::RenderFrameHost* frame = FocusedFrame();
  if (!frame->IsRenderFrameLive()) {
    return;
  }
  const std::string id = FrameId(*frame);
  if (id != composition_read_frame_) {
    composition_read_frame_ = id;
    ++composition_generation_;
  }
  if (composition_read_generation_ == composition_generation_) {
    return;
  }
  composition_read_generation_ = composition_generation_;
  Agent(*frame).ReadComposition(mojo::WrapCallbackWithDefaultInvokeIfNotRun(
      base::BindOnce(&Tab::OnCompositionRead, weak_factory_.GetWeakPtr(),
                     composition_generation_, id),
      mojom::ImeLayout::New()));
}

void Tab::OnCompositionRead(int generation,
                            const std::string& frame,
                            mojom::ImeLayoutPtr layout) {
  if (generation != composition_generation_) {
    return;
  }
  composition_read_generation_.reset();
  // Focus has left it since it was asked, and the next tick asks the frame that has it.
  if (frame != FrameId(*FocusedFrame())) {
    return;
  }
  const bool composing = layout->composing;
  base::DictValue event;
  SetLayout(FrameFromId(frame), *layout, event);
  if (composition_sent_ != event) {
    composition_sent_ = event.Clone();
    SendEvent("composition_layout", std::move(event));
  }
  if (!composing) {
    composition_watch_.Stop();
  }
}

void Tab::DidStartLoading() {
  ReportLoadingState();
}

void Tab::DidStopLoading() {
  ReportLoadingState();
  // A first navigation that stopped without committing leaves nothing to come back to.
  SaveSoon();
}

void Tab::ReportLoadingState() {
  content::NavigationController& controller = contents_->GetController();
  SendEvent("loading_state",
            base::DictValue()
                .Set("is_loading", contents_->IsLoading())
                .Set("can_go_back", controller.CanGoBack())
                .Set("can_go_forward", controller.CanGoForward()));
}

void Tab::DidFinishLoad(content::RenderFrameHost* render_frame_host,
                        const GURL& validated_url) {
  if (!render_frame_host->IsInPrimaryMainFrame()) {
    return;
  }
  if (!first_paint_ && !first_paint_redraw_requested_) {
    first_paint_redraw_requested_ = true;
    StartRedraw();
  }
  content::NavigationEntry* entry =
      contents_->GetController().GetLastCommittedEntry();
  SendEvent("loaded",
            base::DictValue()
                .Set("url", validated_url.spec())
                .Set("http_status", entry ? entry->GetHttpStatusCode() : 0));
}

void Tab::DidFinishNavigation(content::NavigationHandle* handle) {
  if (handle->HasCommitted() && !handle->IsSameDocument()) {
    if (handle->IsInPrimaryMainFrame()) {
      SendEvent("frame_gone", base::DictValue());
      composition_watch_.Stop();
    } else {
      SendEvent("frame_gone",
                base::DictValue().Set(
                    "frame", FrameId(*handle->GetRenderFrameHost())));
    }
  }
  if (handle->IsInPrimaryMainFrame() && handle->HasCommitted()) {
    // A page that never finishes loading or painting still shows what it has.
    if (!first_paint_ && !first_paint_timeout_.IsRunning()) {
      first_paint_timeout_.Start(FROM_HERE, base::Seconds(2), this,
                                 &Tab::OpenFirstFrameGate);
    }
    SendEvent("navigation", base::DictValue().Set("url", handle->GetURL().spec()));
    ReportLoadingState();
  }
}

void Tab::PrimaryPageChanged(content::Page& page) {
  renderer_gone_ = false;
  // The redraw's answer now concerns a page that's gone, and may never come.
  redraw_.reset();
  first_paint_redraw_running_ = false;
  for (Fence& fence : fences_) {
    fence.in_redraw = false;
  }
  StartRedraw();
}

void Tab::DidFirstVisuallyNonEmptyPaint() {
  OpenFirstFrameGate();
}

void Tab::OpenFirstFrameGate() {
  first_paint_ = true;
  // From the first paint, as with viewers.
  if (recording_waiting_) {
    recorder_ = std::make_unique<Recorder>(*view_, *recording_waiting_);
    recording_waiting_.reset();
  }
  first_paint_redraw_requested_ = false;
  first_paint_timeout_.Stop();
  // An unchanged surface produces no more captures, and the last was discarded here: refresh until
  // a frame arrives.
  capturer_->RequestRefreshFrame();
  if (!last_counter_ && !fence_refresh_.IsRunning()) {
    fence_refresh_.Start(FROM_HERE, base::Milliseconds(100), this,
                         &Tab::RefreshForFences);
  }
}

void Tab::TitleWasSet(content::NavigationEntry* entry) {
  SaveSoon();
  if (entry) {
    SendEvent("title", base::DictValue().Set(
                           "title", base::UTF16ToUTF8(entry->GetTitle())));
  }
}

void Tab::PrimaryMainFrameRenderProcessGone(base::TerminationStatus status) {
  renderer_gone_ = true;
  SendEvent("frame_gone", base::DictValue());
  composition_watch_.Stop();
  CloseDialogs();
  std::vector<Fence> fences = std::move(fences_);
  fences_.clear();
  fence_refresh_.Stop();
  redraw_.reset();
  for (const Fence& fence : fences) {
    SendEvent("fence_failed", base::DictValue()
                                  .Set("token", fence.token)
                                  .Set("reason", "renderer terminated"));
  }
}

content::WebContents* Tab::AddNewContents(
    content::WebContents* source,
    std::unique_ptr<content::WebContents> new_contents,
    const GURL& target_url,
    WindowOpenDisposition disposition,
    const blink::mojom::WindowFeatures& window_features,
    bool user_gesture,
    bool* was_blocked) {
  gfx::Size size = size_;
  if (window_features.has_width && window_features.has_height) {
    size = FitDisplay(gfx::Size(window_features.bounds.width(),
                                window_features.bounds.height()),
                      size_);
  }
  return parts_->AdoptPopup(*this, std::move(new_contents), size);
}

// A popup past the host's bound isn't created, as CreateTab refuses a tab past it. Overriding
// creation and leaving CreateCustomWebContents to return nothing suppresses it.
bool Tab::IsWebContentsCreationOverridden(
    content::RenderFrameHost* opener,
    content::SiteInstance* source_site_instance,
    content::mojom::WindowContainerType window_container_type,
    const GURL& opener_url,
    const std::string& frame_name,
    const GURL& target_url) {
  return !parts_->HasRoomForTab();
}

void Tab::WebContentsCreated(content::WebContents* source_contents,
                             const content::GlobalRenderFrameHostId& opener_id,
                             const std::string& frame_name,
                             const GURL& target_url,
                             content::WebContents* new_contents) {
  parts_->PopupCreating(new_contents);
}

void Tab::CloseContents(content::WebContents* source) {
  parts_->CloseTabSoon(id_);
}

content::JavaScriptDialogManager* Tab::GetJavaScriptDialogManager(
    content::WebContents* source) {
  return this;
}

void Tab::RunJavaScriptDialog(content::WebContents* web_contents,
                              content::RenderFrameHost* render_frame_host,
                              content::JavaScriptDialogType dialog_type,
                              const std::u16string& message_text,
                              const std::u16string& default_prompt_text,
                              DialogClosedCallback callback,
                              bool* did_suppress_message) {
  const int request = parts_->NextDialog();
  const bool blocks = BlocksMainFrame(render_frame_host);
  dialogs_[request] = {std::move(callback), blocks};
  SendEvent("dialog", base::DictValue()
                          .Set("request", request)
                          .Set("kind", static_cast<int>(dialog_type))
                          .Set("message", base::UTF16ToUTF8(message_text))
                          .Set("default_text",
                               base::UTF16ToUTF8(default_prompt_text)));
  if (blocks) {
    AnswerFencesForDialog();
  }
}

bool Tab::BlocksMainFrame(content::RenderFrameHost* frame) const {
  return frame->GetProcess() == contents_->GetPrimaryMainFrame()->GetProcess();
}

// Fences waiting on the renderer a dialog blocks can no longer be drawn, so they take the latest
// frame. The Dialog event precedes them.
void Tab::AnswerFencesForDialog() {
  std::vector<Fence> fences = std::move(fences_);
  fences_.clear();
  fence_refresh_.Stop();
  for (const Fence& fence : fences) {
    AnswerFence(fence.token);
  }
}

void Tab::RunBeforeUnloadDialog(content::WebContents* web_contents,
                                content::RenderFrameHost* render_frame_host,
                                bool is_reload,
                                DialogClosedCallback callback) {
  const int request = parts_->NextDialog();
  const bool blocks = BlocksMainFrame(render_frame_host);
  dialogs_[request] = {std::move(callback), blocks};
  SendEvent("dialog", base::DictValue()
                          .Set("request", request)
                          .Set("kind", 3)
                          .Set("message", "")
                          .Set("default_text", ""));
  if (blocks) {
    AnswerFencesForDialog();
  }
}

void Tab::CancelDialogs(content::WebContents* web_contents, bool reset_state) {
  CloseDialogs();
}

void Tab::CloseDialogs() {
  std::map<int, Dialog> dialogs = std::move(dialogs_);
  dialogs_.clear();
  for (auto& [request, dialog] : dialogs) {
    SendEvent("dialog_closed", base::DictValue().Set("request", request));
    std::move(dialog.callback).Run(false, std::u16string());
  }
}

void Tab::OnFrameCaptured(
    media::mojom::VideoBufferHandlePtr data,
    media::mojom::VideoFrameInfoPtr info,
    const gfx::Rect& content_rect,
    mojo::PendingRemote<viz::mojom::FrameSinkVideoConsumerFrameCallbacks>
        callbacks) {
  mojo::Remote<viz::mojom::FrameSinkVideoConsumerFrameCallbacks> done(
      std::move(callbacks));
  // The root window has frames before the page has painted, and they must not look like the page
  // changing: a viewer's first frame is its first paint.
  const std::optional<base::TimeTicks> began = info->metadata.capture_begin_time;
  if (!first_paint_ && first_paint_drawn_ && began && *began >= *first_paint_drawn_) {
    OpenFirstFrameGate();
  }
  if (!first_paint_) {
    done->Done();
    return;
  }
  CHECK(data->is_read_only_shmem_region());
  const base::ReadOnlySharedMemoryRegion& region =
      data->get_read_only_shmem_region();
  base::ReadOnlySharedMemoryMapping& mapping = mappings_[region.GetGUID()];
  if (!mapping.IsValid()) {
    // The pool is small, and replaced when the frame size changes.
    if (mappings_.size() > 16) {
      base::ReadOnlySharedMemoryMapping kept = region.Map();
      mappings_.clear();
      mappings_[region.GetGUID()] = std::move(kept);
    } else {
      mapping = region.Map();
    }
  }
  const base::ReadOnlySharedMemoryMapping& pixels_mapping =
      mappings_[region.GetGUID()];
  if (!pixels_mapping.IsValid()) {
    mappings_.erase(region.GetGUID());
    done->Done();
    return;
  }
  const int stride = media::VideoFrame::RowBytes(
      media::VideoFrame::Plane::kARGB, info->pixel_format,
      info->coded_size.width());
  // The captured image is `content_rect` of the frame; the rest would be letterboxing, which
  // matching resolution constraints should never add.
  const gfx::Rect visible = content_rect;
  const size_t offset =
      static_cast<size_t>(visible.y()) * stride + static_cast<size_t>(visible.x()) * 4;
  TabView::Picture picture{
      .pixels = UNSAFE_TODO(pixels_mapping.GetMemoryAs<uint8_t>() + offset),
      .stride = static_cast<size_t>(stride),
      .size = visible.size(),
      .rgba = info->pixel_format == media::PIXEL_FORMAT_ABGR,
      .update = info->metadata.capture_update_rect.value_or(visible),
  };
  picture.update.Intersect(visible);
  picture.update.Offset(-visible.x(), -visible.y());
  // What the page itself changed; the popups Compose adds ask for captures of their own.
  const bool page_changed = !picture.update.IsEmpty();
  view_->Compose(picture);
  const uint32_t counter =
      static_cast<uint32_t>(info->metadata.capture_counter.value_or(0));
  const bool gap = last_counter_ && counter != *last_counter_ + 1;
  last_counter_ = counter;
  DeliverFrame(LanescapeBrowserFrame{
      .tab = static_cast<uint64_t>(id_),
      .width = static_cast<uint32_t>(visible.width()),
      .height = static_cast<uint32_t>(visible.height()),
      .stride = static_cast<uint32_t>(picture.stride),
      .pixels = picture.pixels,
      .update_x = static_cast<uint32_t>(picture.update.x()),
      .update_y = static_cast<uint32_t>(picture.update.y()),
      .update_width = static_cast<uint32_t>(picture.update.width()),
      .update_height = static_cast<uint32_t>(picture.update.height()),
      .sequence_gap = gap,
      .byte_order = picture.rgba ? uint8_t{1} : uint8_t{0},
  });
  done->Done();

  // A capture after this one, unless one with a change comes first, catches a change the capturer
  // missed (TabView::TrailingRefresh).
  if (const base::TimeDelta trailing = view_->TrailingRefresh();
      trailing.is_positive() && page_changed) {
    trailing_refresh_.Start(FROM_HERE, trailing, this, &Tab::PictureChanged);
  }

  // A fence that arrived before the tab's first frame can start its redraw now.
  StartRedraw();
  // This frame holds what a fence promised if its capture began after the redraw finished.
  std::vector<int> answered;
  std::erase_if(fences_, [&](const Fence& fence) {
    if (fence.drawn && began && *began >= *fence.drawn) {
      answered.push_back(fence.token);
      return true;
    }
    return false;
  });
  // This frame is the tab's first, or a later one, so nothing still waits on first paint.
  if (fences_.empty()) {
    fence_refresh_.Stop();
  }
  for (int token : answered) {
    AnswerFence(token);
  }
  UpdateCaptureRate();
}

void Tab::OnNewCaptureVersion(const media::CaptureVersion&) {}
void Tab::OnFrameWithEmptyRegionCapture() {}
void Tab::OnStopped() {}
void Tab::OnLog(const std::string& message) {
  VLOG(1) << "capture: " << message;
}

}  // namespace lanescape_browser
