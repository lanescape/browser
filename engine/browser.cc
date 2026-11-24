#include "engine/browser.h"

#include <algorithm>
#include <limits>
#include <utility>

#include "base/barrier_closure.h"
#include "base/check.h"
#include "base/compiler_specific.h"
#include "base/functional/bind.h"
#include "base/json/json_reader.h"
#include "base/json/json_writer.h"
#include "base/no_destructor.h"
#include "base/numerics/safe_conversions.h"
#include "base/run_loop.h"
#include "base/task/bind_post_task.h"
#include "base/strings/utf_string_conversions.h"
#include "base/task/single_thread_task_runner.h"
#include "base/values.h"
#include "components/keyed_service/content/browser_context_dependency_manager.h"
#include "content/public/browser/browser_task_traits.h"
#include "content/public/browser/browser_thread.h"
#include "content/public/browser/devtools_agent_host.h"
#include "content/public/browser/dom_storage_context.h"
#include "content/public/browser/session_storage_namespace_handle.h"
#include "content/public/browser/devtools_agent_host_client.h"
#include "content/public/browser/render_frame_host.h"
#include "content/public/browser/storage_partition.h"
#include "content/public/browser/web_contents.h"
#include "content/public/browser/web_contents_observer.h"
#include "content/public/common/result_codes.h"
#include "mojo/public/cpp/bindings/callback_helpers.h"
#include "engine/platform.h"
#include "engine/tab.h"
#include "services/network/public/mojom/cookie_manager.mojom.h"
#include "ui/base/clipboard/clipboard.h"
#include "ui/display/screen.h"
#include "ui/display/screen_base.h"
#include "ui/events/devices/device_data_manager.h"
#include "url/gurl.h"

namespace lanescape_browser {

namespace {

LanescapeBrowserHost g_host{};
BrowserMainParts* g_main_parts = nullptr;

// One large display at scale 1, so a popup placed anywhere in a tab stays on screen. Tab sizes
// come from their own views, not from this.
class Screen : public display::ScreenBase {
 public:
  Screen() {
    ProcessDisplayChanged(
        display::Display(DisplayId(),
                         gfx::Rect(DisplayOrigin(), gfx::Size(8192, 8192))),
        /*is_primary=*/true);
  }

  gfx::Point GetCursorScreenPoint() override { return gfx::Point(); }
  bool IsWindowUnderCursor(gfx::NativeWindow window) override { return false; }
  gfx::NativeWindow GetWindowAtScreenPoint(const gfx::Point& point) override {
    return gfx::NativeWindow();
  }
  gfx::NativeWindow GetLocalProcessWindowAtPoint(
      const gfx::Point& point,
      const std::set<gfx::NativeWindow>& ignore) override {
    return gfx::NativeWindow();
  }
  display::Display GetDisplayNearestWindow(
      gfx::NativeWindow window) const override {
    return GetPrimaryDisplay();
  }
};

}  // namespace

void SetHost(const LanescapeBrowserHost& host) {
  g_host = host;
}

namespace {
std::string& DataDirectoryStorage() {
  static base::NoDestructor<std::string> path;
  return *path;
}
}  // namespace

void SetDataDirectory(std::string path) {
  DataDirectoryStorage() = std::move(path);
}

const std::string& DataDirectory() {
  return DataDirectoryStorage();
}

// These calls cross into the host, built outside Chromium, which control-flow integrity can't check.
NO_SANITIZE("cfi-icall")
void Emit(const std::string& json) {
  g_host.on_event(json.data(), json.size());
}

NO_SANITIZE("cfi-icall")
void DeliverFrame(const LanescapeBrowserFrame& frame) {
  g_host.on_frame(&frame);
}

NO_SANITIZE("cfi-icall")
void DeliverRecordingFrame(uint64_t recording,
                           base::span<const uint8_t> data,
                           int64_t pts,
                           bool key,
                           uint32_t width,
                           uint32_t height) {
  g_host.on_recording(recording, data.data(), data.size(), pts, key ? 1 : 0,
                      width, height);
}

NO_SANITIZE("cfi-icall")
void DeliverRecordingEnd(uint64_t recording) {
  g_host.on_recording(recording, nullptr, 0, 0, 0, 0, 0);
}

NO_SANITIZE("cfi-icall")
void DeliverDevTools(uint64_t session, base::span<const uint8_t> message) {
  g_host.on_devtools(session, message.data(), message.size());
}

NO_SANITIZE("cfi-icall")
static void NotifyStarted() {
  g_host.on_started();
}

void PostCommand(std::string json) {
  content::GetUIThreadTaskRunner({})->PostTask(
      FROM_HERE, base::BindOnce(
                     [](std::string command) {
                       if (g_main_parts) {
                         g_main_parts->HandleCommand(command);
                       }
                     },
                     std::move(json)));
}

void PostDevTools(uint64_t session,
                  uint64_t id,
                  bool is_input,
                  std::string message) {
  content::GetUIThreadTaskRunner({})->PostTask(
      FROM_HERE,
      base::BindOnce(
          [](uint64_t session, uint64_t id, bool is_input, std::string message) {
            if (g_main_parts) {
              g_main_parts->SendDevTools(session, id, is_input, message);
            }
          },
          session, id, is_input, std::move(message)));
}

// One client on a tab's agent host. Attaching per client means Chromium's own session teardown
// undoes what the client set, and these answers hold it to what a page can reach
// (specs/0002-control.md, DevTools sessions).
class DevToolsSession : public content::DevToolsAgentHostClient {
 public:
  DevToolsSession(uint64_t id, int tab, content::WebContents* contents)
      : id_(id),
        tab_(tab),
        agent_(content::DevToolsAgentHost::GetOrCreateFor(contents)) {
    agent_->AttachClient(this);
  }

  ~DevToolsSession() override {
    if (agent_) {
      agent_->DetachClient(this);
    }
  }

  int tab() const { return tab_; }

  void Send(const std::string& message) {
    if (agent_) {
      agent_->DispatchProtocolMessage(this, base::as_byte_span(message));
    }
  }

  // content::DevToolsAgentHostClient:
  void DispatchProtocolMessage(content::DevToolsAgentHost* agent_host,
                               base::span<const uint8_t> message) override {
    DeliverDevTools(id_, message);
  }
  void AgentHostClosed(content::DevToolsAgentHost* agent_host) override {
    agent_ = nullptr;
  }
  bool MayReadLocalFiles() override { return false; }
  bool MayWriteLocalFiles() override { return false; }
  bool AllowUnsafeOperations() override { return false; }

 private:
  const uint64_t id_;
  const int tab_;
  scoped_refptr<content::DevToolsAgentHost> agent_;
};

BrowserContext::BrowserContext(base::FilePath path) : path_(std::move(path)) {
  BrowserContextDependencyManager::GetInstance()->CreateBrowserContextServices(
      this);
}

BrowserContext::~BrowserContext() {
  BrowserContextDependencyManager::GetInstance()
      ->DestroyBrowserContextServices(this);
  NotifyWillBeDestroyed();
  ShutdownStoragePartitions();
}

base::FilePath BrowserContext::GetPath() const {
  return path_;
}
std::unique_ptr<content::ZoomLevelDelegate>
BrowserContext::CreateZoomLevelDelegate(const base::FilePath&) {
  return nullptr;
}
bool BrowserContext::IsOffTheRecord() {
  return false;
}
content::DownloadManagerDelegate* BrowserContext::GetDownloadManagerDelegate() {
  return nullptr;
}
content::BrowserPluginGuestManager* BrowserContext::GetGuestManager() {
  return nullptr;
}
storage::SpecialStoragePolicy* BrowserContext::GetSpecialStoragePolicy() {
  return nullptr;
}
content::PlatformNotificationService*
BrowserContext::GetPlatformNotificationService() {
  return nullptr;
}
content::PushMessagingService* BrowserContext::GetPushMessagingService() {
  return nullptr;
}
content::StorageNotificationService*
BrowserContext::GetStorageNotificationService() {
  return nullptr;
}
content::SSLHostStateDelegate* BrowserContext::GetSSLHostStateDelegate() {
  return nullptr;
}
content::PermissionControllerDelegate*
BrowserContext::GetPermissionControllerDelegate() {
  return &permissions_;
}
content::ClientHintsControllerDelegate*
BrowserContext::GetClientHintsControllerDelegate() {
  return nullptr;
}
content::BackgroundFetchDelegate* BrowserContext::GetBackgroundFetchDelegate() {
  return nullptr;
}
content::BackgroundSyncController*
BrowserContext::GetBackgroundSyncController() {
  return nullptr;
}
content::BrowsingDataRemoverDelegate*
BrowserContext::GetBrowsingDataRemoverDelegate() {
  return nullptr;
}
content::ReduceAcceptLanguageControllerDelegate*
BrowserContext::GetReduceAcceptLanguageControllerDelegate() {
  return nullptr;
}
content::OriginTrialsControllerDelegate*
BrowserContext::GetOriginTrialsControllerDelegate() {
  return nullptr;
}

// Asks the browser target what compositing actually runs. A launch flag alone is no proof of
// hardware acceleration.
class BrowserMainParts::GpuStatusClient : public content::DevToolsAgentHostClient {
 public:
  GpuStatusClient()
      : agent_(content::DevToolsAgentHost::CreateForBrowser(
            nullptr,
            content::DevToolsAgentHost::CreateServerSocketCallback())) {
    agent_->AttachClient(this);
  }
  ~GpuStatusClient() override {
    if (agent_) {
      agent_->DetachClient(this);
    }
  }

  void Ask(int request) {
    if (!agent_) {
      return;
    }
    std::optional<std::string> message = base::WriteJson(
        base::DictValue().Set("id", request).Set("method", "SystemInfo.getInfo"));
    CHECK(message);
    agent_->DispatchProtocolMessage(this, base::as_byte_span(*message));
  }

  // content::DevToolsAgentHostClient:
  void DispatchProtocolMessage(content::DevToolsAgentHost* agent_host,
                               base::span<const uint8_t> message) override {
    std::optional<base::DictValue> reply = base::JSONReader::ReadDict(
        base::as_string_view(message), base::JSON_PARSE_CHROMIUM_EXTENSIONS);
    if (!reply || !reply->FindInt("id")) {
      return;
    }
    base::DictValue event;
    event.Set("event", "gpu_status");
    event.Set("request", *reply->FindInt("id"));
    if (const base::DictValue* gpu = reply->FindDictByDottedPath("result.gpu")) {
      event.Set("gpu", gpu->Clone());
    }
    std::optional<std::string> json = base::WriteJson(event);
    CHECK(json);
    Emit(*json);
  }
  void AgentHostClosed(content::DevToolsAgentHost* agent_host) override {
    agent_ = nullptr;
  }
  bool MayReadLocalFiles() override { return false; }
  bool MayWriteLocalFiles() override { return false; }

 private:
  scoped_refptr<content::DevToolsAgentHost> agent_;
};

// A popup Chromium holds until it shows it, which may never happen.
class BrowserMainParts::PendingPopup : public content::WebContentsObserver {
 public:
  PendingPopup(BrowserMainParts& parts, content::WebContents* contents)
      : WebContentsObserver(contents), parts_(parts), contents_(contents) {}

  void WebContentsDestroyed() override {
    parts_->pending_popups_.erase(contents_);  // Deletes this.
  }

 private:
  const raw_ref<BrowserMainParts> parts_;
  const raw_ptr<content::WebContents> contents_;
};

BrowserMainParts::BrowserMainParts() {
  CHECK(!g_main_parts);
  g_main_parts = this;
}

BrowserMainParts::~BrowserMainParts() {
  g_main_parts = nullptr;
}

void BrowserMainParts::ToolkitInitialized() {
  static base::NoDestructor<Screen> screen;
  display::Screen::SetScreenInstance(screen.get());
  ui::DeviceDataManager::CreateInstance();
}

int BrowserMainParts::PreMainMessageLoopRun() {
  ui::Clipboard::SetClipboardForCurrentThread(CreateInertClipboard());
  NotifyStarted();
  return content::RESULT_CODE_NORMAL_EXIT;
}

void BrowserMainParts::WillRunMainMessageLoop(
    std::unique_ptr<base::RunLoop>& run_loop) {
  quit_ = run_loop->QuitClosure();
}

void BrowserMainParts::SendDevTools(uint64_t session,
                                    uint64_t id,
                                    bool is_input,
                                    const std::string& message) {
  auto it = devtools_.find(session);
  if (it == devtools_.end()) {
    return;
  }
  // The socket thread checked this already, but CONTROL may have moved while this was queued.
  if (is_input) {
    auto tab = tabs_.find(it->second->tab());
    if (tab == tabs_.end() || tab->second->controlled()) {
      std::optional<std::string> reply = base::WriteJson(
          base::DictValue()
              .Set("event", "devtools_refused")
              .Set("session", base::checked_cast<int>(session))
              .Set("id", base::checked_cast<int>(id)));
      CHECK(reply);
      Emit(*reply);
      return;
    }
  }
  it->second->Send(message);
}

bool BrowserMainParts::HasRoomForTab(bool new_id) const {
  // Tab IDs are never reused, so a data directory has a last one; a restored tab has its own.
  return tabs_.size() + pending_popups_.size() < 128 &&
         (!new_id || next_tab_ + static_cast<int>(pending_popups_.size()) <
                         std::numeric_limits<int>::max());
}

void BrowserMainParts::PopupCreating(content::WebContents* contents) {
  pending_popups_[contents] = std::make_unique<PendingPopup>(*this, contents);
}

void BrowserMainParts::PostMainMessageLoopRun() {
  gpu_status_.reset();
  pending_popups_.clear();
  devtools_.clear();
  tabs_.clear();
  contexts_.clear();
}

void BindFrameHost(content::GlobalRenderFrameHostId frame,
                   mojo::PendingAssociatedReceiver<mojom::FrameHost> receiver) {
  if (g_main_parts) {
    g_main_parts->BindFrame(frame, std::move(receiver));
  }
}

void BrowserMainParts::BindFrame(
    content::GlobalRenderFrameHostId frame,
    mojo::PendingAssociatedReceiver<mojom::FrameHost> receiver) {
  frame_hosts_.Add(this, std::move(receiver), frame);
}

Tab* BrowserMainParts::TabOfCurrentFrame(content::RenderFrameHost** frame) {
  *frame = content::RenderFrameHost::FromID(frame_hosts_.current_context());
  if (!*frame) {
    return nullptr;
  }
  content::WebContents* contents =
      content::WebContents::FromRenderFrameHost(*frame);
  for (const auto& [id, tab] : tabs_) {
    if (tab->contents() == contents) {
      return tab.get();
    }
  }
  // A popup not handed over yet; its page has no viewer to report to.
  return nullptr;
}

void BrowserMainParts::FocusChanged(int32_t counter,
                                    bool editable,
                                    const gfx::Rect& bounds) {
  content::RenderFrameHost* frame = nullptr;
  if (Tab* tab = TabOfCurrentFrame(&frame)) {
    tab->FocusChanged(*frame, counter, editable, bounds);
  }
}

void BrowserMainParts::CommandStarted(int32_t token) {
  content::RenderFrameHost* frame = nullptr;
  if (Tab* tab = TabOfCurrentFrame(&frame)) {
    tab->CommandStarted(*frame, token);
  }
}

void BrowserMainParts::CommandFinished(int32_t token,
                                       mojom::CommandEnd end,
                                       bool selection,
                                       bool editable) {
  content::RenderFrameHost* frame = nullptr;
  if (Tab* tab = TabOfCurrentFrame(&frame)) {
    tab->CommandFinished(*frame, token, end, selection, editable);
  }
}

Tab* BrowserMainParts::FindTab(const base::DictValue& command) {
  auto it = tabs_.find(command.FindInt("tab").value_or(0));
  return it == tabs_.end() ? nullptr : it->second.get();
}

BrowserContext* BrowserMainParts::ContextFor(const std::string& key,
                                             const std::string& path) {
  std::unique_ptr<BrowserContext>& context = contexts_[key];
  if (!context) {
    context = std::make_unique<BrowserContext>(base::FilePath(path));
    context->generation = ++context_generation_;
    // A new unnamed profile has nothing to sweep.
    if (keep_session_storage_ && !key.starts_with("unnamed-")) {
      RequestSweep(key, *context);
    }
  }
  return context.get();
}

// Asks the host to say when to sweep this context: once every tab closed so far is forgotten on
// disk. The answer names this load of the profile, and no other.
void BrowserMainParts::RequestSweep(const std::string& key,
                                    const BrowserContext& context) {
  if (keep_session_storage_->contains(key)) {
    return;
  }
  std::optional<std::string> json =
      base::WriteJson(base::DictValue()
                          .Set("event", "context_loaded")
                          .Set("context", key)
                          .Set("generation", context.generation));
  CHECK(json);
  Emit(*json);
}

// Session storage a closed or discarded tab left: a tab's persists until then, so one that a crash
// brings back still has it. Each context once, when the host has those tabs forgotten on disk.
void BrowserMainParts::ScavengeSessionStorage(BrowserContext& context) {
  context.GetDefaultStoragePartition()
      ->GetDOMStorageContext()
      ->StartScavengingUnusedSessionStorage();
}

content::WebContents* BrowserMainParts::AdoptPopup(
    Tab& opener,
    std::unique_ptr<content::WebContents> contents,
    const gfx::Size& size) {
  const int id = next_tab_++;
  content::WebContents* raw = contents.get();
  pending_popups_.erase(raw);
  tabs_[id] = std::make_unique<Tab>(*this, id, opener.context(),
                                    std::move(contents), size);
  std::optional<std::string> json = base::WriteJson(
      base::DictValue()
          .Set("event", "popup")
          .Set("tab", id)
          .Set("opener", opener.id())
          .Set("context", opener.context()));
  CHECK(json);
  Emit(*json);
  return raw;
}

void BrowserMainParts::CloseTabSoon(int id) {
  content::GetUIThreadTaskRunner({})->PostTask(
      FROM_HERE, base::BindOnce(&BrowserMainParts::CloseTab,
                                weak_factory_.GetWeakPtr(), id));
}

// A restored tab takes up its saved session storage, which Chromium keeps under the namespace's ID.
// Anything that isn't an ID Chromium made gets a new namespace.
std::unique_ptr<content::WebContents> BrowserMainParts::CreateContents(
    const content::WebContents::CreateParams& params,
    const base::DictValue* state) {
  const std::string* id = state ? state->FindString("session_storage") : nullptr;
  if (!id || id->size() != 36 ||
      !std::all_of(id->begin(), id->end(), [](char c) {
        return (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || c == '_';
      })) {
    return content::WebContents::Create(params);
  }
  scoped_refptr<content::SessionStorageNamespaceHandle> session_storage =
      params.browser_context->GetDefaultStoragePartition()
          ->GetDOMStorageContext()
          ->RecreateSessionStorage(*id);
  return content::WebContents::CreateWithSessionStorage(
      params, content::CreateMapWithDefaultSessionStorageNamespace(
                  params.browser_context, session_storage));
}

void BrowserMainParts::CloseTab(int id) {
  std::erase_if(devtools_,
                [&](const auto& entry) { return entry.second->tab() == id; });
  auto it = tabs_.find(id);
  if (it == tabs_.end()) {
    return;
  }
  const std::string context = it->second->context();
  // Closed for good, unlike a tab that shutdown takes down: no later host brings it back. A
  // restored tab the host never admitted stays saved, with its profile.
  const bool keep_saved = it->second->restored() && !it->second->admitted();
  tabs_.erase(it);
  auto emit = [id](const char* event) {
    std::optional<std::string> json =
        base::WriteJson(base::DictValue().Set("event", event).Set("tab", id));
    CHECK(json);
    Emit(*json);
  };
  if (!keep_saved) {
    emit("tab_forget");
  }
  emit("closed");
  // An unnamed profile lives as long as its last tab. Shutdown closes no tab, so it outlasts that.
  if (context.starts_with("unnamed-") && !keep_saved) {
    ReleaseContext(context, 0);
  }
}

// Writes the context's cookies and destroys it, unless a tab or a popup not handed over yet still
// uses it. The host removes an unnamed profile's directory once it hears it was released.
void BrowserMainParts::ReleaseContext(const std::string& key, int request) {
  FlushThen(&key,
            base::BindOnce(
                [](base::WeakPtr<BrowserMainParts> parts, std::string key,
                   int request) {
                  if (!parts) {
                    return;
                  }
                  // A popup can have joined the context since the host checked, and destroying a
                  // context under its WebContents would crash.
                  bool in_use = false;
                  for (const auto& [id, tab] : parts->tabs_) {
                    in_use |= tab->context() == key;
                  }
                  auto context = parts->contexts_.find(key);
                  for (const auto& [contents, popup] : parts->pending_popups_) {
                    in_use |= context != parts->contexts_.end() &&
                              contents->GetBrowserContext() ==
                                  context->second.get();
                  }
                  // Shutdown has taken every tab down and is flushing every context itself;
                  // destroying one now would cut that flush short.
                  if (!in_use && !parts->quitting_) {
                    parts->contexts_.erase(key);
                  }
                  std::optional<std::string> reply = base::WriteJson(
                      base::DictValue()
                          .Set("event", "context_released")
                          .Set("request", request)
                          .Set("context", key)
                          .Set("released", !in_use));
                  CHECK(reply);
                  Emit(*reply);
                },
                weak_factory_.GetWeakPtr(), key, request));
}

// Writes each context's cookies, or one context's when named. Destroying a context can race the
// network service's asynchronous writes.
void BrowserMainParts::FlushThen(const std::string* context,
                                 base::OnceClosure done) {
  std::vector<BrowserContext*> targets;
  for (auto& [key, value] : contexts_) {
    if (!context || key == *context) {
      targets.push_back(value.get());
    }
  }
  base::RepeatingClosure barrier =
      base::BarrierClosure(targets.size(), std::move(done));
  // Destroying a context drops its pending flushes' callbacks, which must still count, or a
  // shutdown racing a release would wait forever. They count as a task of their own, never inside
  // the destruction that dropped them, which may be another release's.
  for (BrowserContext* target : targets) {
    target->GetDefaultStoragePartition()
        ->GetCookieManagerForBrowserProcess()
        ->FlushCookieStore(mojo::WrapCallbackWithDefaultInvokeIfNotRun(
            base::BindPostTaskToCurrentDefault(base::OnceClosure(barrier))));
  }
}

void BrowserMainParts::Quit() {
  // From here on only shutdown destroys contexts, after the flush below.
  quitting_ = true;
  devtools_.clear();
  // Every tab outlasts shutdown: the next host restores what they hold now.
  for (const auto& [id, tab] : tabs_) {
    tab->Save();
  }
  tabs_.clear();
  FlushThen(nullptr, base::BindOnce(
                         [](base::WeakPtr<BrowserMainParts> parts) {
                           if (parts && parts->quit_) {
                             std::move(parts->quit_).Run();
                           }
                         },
                         weak_factory_.GetWeakPtr()));
}

void BrowserMainParts::HandleCommand(const std::string& json) {
  std::optional<base::DictValue> command = base::JSONReader::ReadDict(
      json, base::JSON_PARSE_CHROMIUM_EXTENSIONS);
  CHECK(command);
  const std::string* op = command->FindString("op");
  CHECK(op);
  if (*op == "create_tab") {
    const std::string* key = command->FindString("context");
    const std::string* path = command->FindString("path");
    const std::string* url = command->FindString("url");
    CHECK(key && path && url);
    // A popup may have taken the last room since the host checked.
    if (!HasRoomForTab(!command->FindInt("tab"))) {
      std::optional<std::string> refused = base::WriteJson(
          base::DictValue()
              .Set("event", "tab_refused")
              .Set("request", command->FindInt("request").value_or(0)));
      CHECK(refused);
      Emit(*refused);
      return;
    }
    // A restored tab keeps the ID it had (specs/0002-control.md, Restarts).
    const int id = command->FindInt("tab").value_or(next_tab_);
    if (tabs_.contains(id)) {
      std::optional<std::string> refused = base::WriteJson(
          base::DictValue()
              .Set("event", "tab_refused")
              .Set("request", command->FindInt("request").value_or(0)));
      CHECK(refused);
      Emit(*refused);
      return;
    }
    next_tab_ = std::max(next_tab_, id + 1);
    BrowserContext* context = ContextFor(*key, *path);
    const gfx::Size size(command->FindInt("width").value_or(1280),
                         command->FindInt("height").value_or(800));
    content::WebContents::CreateParams params(context);
    const base::DictValue* state = command->FindDict("state");
    Tab& tab = *(tabs_[id] = std::make_unique<Tab>(
                     *this, id, *key, CreateContents(params, state), size));
    // A restored tab has its history before the host hears of it, and loads only after.
    if (state) {
      tab.Restore(*state);
    }
    std::optional<std::string> reply = base::WriteJson(
        base::DictValue()
            .Set("event", "tab_created")
            .Set("tab", id)
            .Set("request", command->FindInt("request").value_or(0)));
    CHECK(reply);
    Emit(*reply);
    // A restored tab loads once the host admits it.
    if (!state) {
      tab.Load(GURL(*url));
    }
    return;
  }
  if (*op == "scavenge") {
    // Restoring is over, or there was none: each profile drops the session storage no live tab
    // holds, loaded now or later, except profiles with saved tabs still to come back.
    const base::ListValue* keep = command->FindList("keep");
    CHECK(keep);
    keep_session_storage_.emplace();
    for (const base::Value& key : *keep) {
      if (key.is_string()) {
        keep_session_storage_->insert(key.GetString());
      }
    }
    for (const auto& [key, context] : contexts_) {
      RequestSweep(key, *context);
    }
    return;
  }
  if (*op == "scavenge_context") {
    // Only the context the host was told of: one released since may have been replaced.
    const std::string* key = command->FindString("context");
    CHECK(key);
    auto it = contexts_.find(*key);
    if (it != contexts_.end() &&
        it->second->generation == command->FindInt("generation").value_or(0)) {
      ScavengeSessionStorage(*it->second);
    }
    return;
  }
  if (*op == "start") {
    // The first ID no host has handed out, so none is ever handed out twice.
    next_tab_ = std::max(next_tab_, command->FindInt("next_tab").value_or(1));
    return;
  }
  if (*op == "release_context") {
    const std::string* key = command->FindString("context");
    CHECK(key);
    ReleaseContext(*key, command->FindInt("request").value_or(0));
    return;
  }
  if (*op == "quit") {
    Quit();
    return;
  }
  if (*op == "gpu_status") {
    if (!gpu_status_) {
      gpu_status_ = std::make_unique<GpuStatusClient>();
    }
    gpu_status_->Ask(command->FindInt("request").value_or(0));
    return;
  }
  if (*op == "devtools_detach") {
    const std::optional<double> session = command->FindDouble("session");
    CHECK(session);
    devtools_.erase(static_cast<uint64_t>(*session));
    std::optional<std::string> reply = base::WriteJson(
        base::DictValue()
            .Set("event", "devtools_detached")
            .Set("session", base::checked_cast<int>(*session)));
    CHECK(reply);
    Emit(*reply);
    return;
  }
  if (*op == "clipboard_end") {
    const std::string* key = command->FindString("context");
    CHECK(key);
    const int token = command->FindInt("token").value_or(0);
    if (auto it = contexts_.find(*key); it != contexts_.end()) {
      it->second->clipboard().End(token);
    }
    for (const auto& [id, tab] : tabs_) {
      tab->ForgetClipboardCommand(token);
    }
    return;
  }
  Tab* tab = FindTab(*command);
  if (!tab) {
    return;  // It closed while this command was on its way.
  }
  if (*op == "navigate") {
    const std::string* url = command->FindString("url");
    CHECK(url);
    tab->Load(GURL(*url));
  } else if (*op == "back") {
    tab->GoBack();
  } else if (*op == "forward") {
    tab->GoForward();
  } else if (*op == "reload") {
    tab->Reload();
  } else if (*op == "stop") {
    tab->Stop();
  } else if (*op == "resize") {
    tab->Resize(gfx::Size(command->FindInt("width").value_or(1280),
                          command->FindInt("height").value_or(800)));
  } else if (*op == "focus") {
    tab->Focus();
  } else if (*op == "ime") {
    tab->Ime(*command);
  } else if (*op == "ime_forget") {
    tab->ForgetIme(command->FindInt("token").value_or(0));
  } else if (*op == "clipboard_withdraw") {
    tab->WithdrawClipboardCommand(command->FindInt("token").value_or(0));
  } else if (*op == "clipboard_run") {
    tab->RunClipboardCommand(*command);
  } else if (*op == "menu_command") {
    tab->RunMenuCommand(command->FindInt("command").value_or(0));
  } else if (*op == "menu_closed") {
    tab->CloseContextMenu();
  } else if (*op == "record_start") {
    const std::optional<double> recording = command->FindDouble("recording");
    tab->StartRecording(static_cast<uint64_t>(recording.value_or(0)));
  } else if (*op == "record_stop") {
    tab->StopRecording();
  } else if (*op == "record_key_frame") {
    tab->RequestRecordingKeyFrame();
  } else if (*op == "viewed") {
    tab->SetViewed(command->FindBool("viewed").value_or(false));
  } else if (*op == "admit") {
    tab->Admit();
  } else if (*op == "controlled") {
    tab->set_controlled(command->FindBool("controlled").value_or(false));
  } else if (*op == "mouse") {
    tab->Mouse(*command);
  } else if (*op == "wheel") {
    tab->Wheel(*command);
  } else if (*op == "key") {
    tab->Key(*command);
  } else if (*op == "dialog_reply") {
    const std::string* text = command->FindString("text");
    tab->ReplyDialog(command->FindInt("request").value_or(0),
                     command->FindBool("accept").value_or(false),
                     text ? base::UTF8ToUTF16(*text) : std::u16string());
  } else if (*op == "fence") {
    tab->RequestFence(command->FindInt("token").value_or(0));
  } else if (*op == "fence_cancel") {
    tab->CancelFence(command->FindInt("token").value_or(0));
  } else if (*op == "devtools_attach") {
    const std::optional<double> session = command->FindDouble("session");
    CHECK(session);
    devtools_[static_cast<uint64_t>(*session)] =
        std::make_unique<DevToolsSession>(static_cast<uint64_t>(*session),
                                          tab->id(), tab->contents());
  } else if (*op == "sandbox") {
    tab->ReportSandbox();
  } else if (*op == "close") {
    CloseTab(tab->id());
  } else {
    LOG(ERROR) << "unknown engine command";
  }
}

}  // namespace lanescape_browser
