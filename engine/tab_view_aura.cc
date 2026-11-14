// A tab's view on Linux: an Aura window tree host with no platform window behind it, under headless
// Ozone.
#include <memory>
#include <optional>
#include <string>

#include "base/containers/flat_map.h"
#include "base/containers/flat_set.h"
#include "base/memory/raw_ptr.h"
#include "base/memory/raw_ref.h"
#include "base/observer_list.h"
#include "content/public/browser/web_contents.h"
#include "content/public/browser/web_contents_view_delegate.h"
#include "lanescape_browser/tab_view.h"
#include "ui/aura/client/default_capture_client.h"
#include "ui/aura/client/focus_change_observer.h"
#include "ui/aura/client/focus_client.h"
#include "ui/aura/client/window_parenting_client.h"
#include "ui/aura/window.h"
#include "ui/aura/window_observer.h"
#include "ui/aura/window_tree_host.h"
#include "ui/base/ime/input_method.h"
#include "ui/base/ime/input_method_observer.h"
#include "ui/base/ime/text_input_client.h"
#include "ui/events/event.h"
#include "ui/events/event_sink.h"
#include "ui/events/keycodes/dom/dom_code.h"
#include "ui/events/platform/platform_event_dispatcher.h"
#include "ui/gfx/geometry/rect.h"

namespace lanescape_browser {

namespace {

// A window tree host with no platform window behind it. Each tab has its own, so tab sizes are
// independent and capture has one stable target across the views a navigation swaps in.
class TabHost : public aura::WindowTreeHost,
                public ui::PlatformEventDispatcher,
                public aura::client::FocusClient,
                public aura::client::WindowParentingClient,
                public aura::WindowObserver {
 public:
  explicit TabHost(const gfx::Size& size) : bounds_(size) {
    CreateCompositor(/*force_software_compositor=*/false,
                     /*use_external_begin_frame_control=*/false);
    OnAcceleratedWidgetAvailable();
    aura::client::SetFocusClient(window(), this);
    aura::client::SetWindowParentingClient(window(), this);
    // <select> popups grab the mouse through this.
    capture_client_ =
        std::make_unique<aura::client::DefaultCaptureClient>(window());
  }

  ~TabHost() override {
    capture_client_.reset();
    aura::client::SetWindowParentingClient(window(), nullptr);
    aura::client::SetFocusClient(window(), nullptr);
    if (focused_) {
      focused_->RemoveObserver(this);
      focused_ = nullptr;
    }
    DestroyCompositor();
    DestroyDispatcher();
  }

  // ui::PlatformEventDispatcher:
  bool CanDispatchEvent(const ui::PlatformEvent&) override { return false; }
  uint32_t DispatchEvent(const ui::PlatformEvent&) override { return 0; }

  // aura::WindowTreeHost:
  ui::EventSource* GetEventSource() override { return this; }
  gfx::AcceleratedWidget GetAcceleratedWidget() override {
    return gfx::AcceleratedWidget();
  }
  void ShowImpl() override {}
  void HideImpl() override {}
  gfx::Rect GetBoundsInPixels() const override { return bounds_; }
  void SetBoundsInPixels(const gfx::Rect& bounds) override {
    window()->SetBounds(bounds);
    if (bounds_ != bounds) {
      bounds_ = bounds;
      OnHostResizedInPixels(bounds.size());
    }
  }
  gfx::Point GetLocationOnScreenInPixels() const override {
    return bounds_.origin();
  }
  void SetCapture() override {}
  void ReleaseCapture() override {}
  bool CaptureSystemKeyEventsImpl(
      std::optional<base::flat_set<ui::DomCode>>) override {
    return false;
  }
  void ReleaseSystemKeyEventCapture() override {}
  bool IsKeyLocked(ui::DomCode) override { return false; }
  base::flat_map<std::string, std::string> GetKeyboardLayoutMap() override {
    return {};
  }
  void SetCursorNative(gfx::NativeCursor) override {}
  void MoveCursorToScreenLocationInPixels(const gfx::Point&) override {}
  void OnCursorVisibilityChangedNative(bool) override {}

  // aura::client::FocusClient:
  void AddObserver(aura::client::FocusChangeObserver* observer) override {
    focus_observers_.AddObserver(observer);
  }
  void RemoveObserver(aura::client::FocusChangeObserver* observer) override {
    focus_observers_.RemoveObserver(observer);
  }
  void FocusWindow(aura::Window* window) override {
    if (window && !window->CanFocus()) {
      return;
    }
    aura::Window* old = focused_;
    if (old) {
      old->RemoveObserver(this);
    }
    focused_ = window;
    if (focused_) {
      focused_->AddObserver(this);
    }
    for (aura::client::FocusChangeObserver& observer : focus_observers_) {
      observer.OnWindowFocused(focused_, old);
    }
    if (auto* observer = aura::client::GetFocusChangeObserver(old)) {
      observer->OnWindowFocused(focused_, old);
    }
    if (auto* observer = aura::client::GetFocusChangeObserver(focused_)) {
      observer->OnWindowFocused(focused_, old);
    }
  }
  void ResetFocusWithinActiveWindow(aura::Window* window) override {
    if (!window->Contains(focused_)) {
      FocusWindow(window);
    }
  }
  aura::Window* GetFocusedWindow() override { return focused_; }

  // aura::client::WindowParentingClient:
  aura::Window* GetDefaultParent(aura::Window*,
                                 const gfx::Rect&,
                                 int64_t) override {
    return window();
  }

  // aura::WindowObserver:
  void OnWindowDestroying(aura::Window*) override { FocusWindow(nullptr); }

 private:
  gfx::Rect bounds_;
  raw_ptr<aura::Window> focused_ = nullptr;
  base::ObserverList<aura::client::FocusChangeObserver> focus_observers_;
  std::unique_ptr<aura::client::DefaultCaptureClient> capture_client_;
};

class AuraTabView : public TabView, public ui::InputMethodObserver {
 public:
  AuraTabView(content::WebContents& contents,
              const gfx::Size& size,
              Delegate& delegate)
      : contents_(&contents),
        delegate_(&delegate),
        host_(std::make_unique<TabHost>(size)) {
    host_->InitHost();
    aura::Window* root = host_->window();
    root->Show();
    host_->Show();
    gfx::NativeView view = contents.GetNativeView();
    root->AddChild(view);
    view->Show();
    const gfx::Rect bounds(size);
    view->SetBounds(bounds);
    host_->SetBoundsInPixels(bounds);
    host_->GetInputMethod()->AddObserver(this);
  }

  ~AuraTabView() override {
    Detach();
    host_.reset();
  }

  // TabView:
  void Detach() override {
    if (delegate_) {
      host_->GetInputMethod()->RemoveObserver(this);
      delegate_ = nullptr;
    }
    contents_ = nullptr;
  }
  void Resize(const gfx::Size& size) override {
    const gfx::Rect bounds(size);
    contents_->GetNativeView()->SetBounds(bounds);
    host_->SetBoundsInPixels(bounds);
  }
  // The root's frame sink, not a view's: a navigation can swap views, and only the root includes
  // <select> popups, which are separate widgets.
  viz::FrameSinkId CaptureTarget() override {
    return host_->window()->GetFrameSinkId();
  }
  // The root already has its popups, and its captures follow every change.
  void Compose(Picture& picture) override {}
  base::TimeDelta TrailingRefresh() override { return base::TimeDelta(); }
  void AddPopupOverlays(viz::ClientFrameSinkVideoCapturer& capturer) override {}
  void RemovePopupOverlays(
      viz::ClientFrameSinkVideoCapturer& capturer) override {}
  // Through the window tree, not RenderWidgetHost::Forward*, so hit testing reaches out-of-process
  // iframes and popups.
  void Mouse(ui::MouseEvent& event) override {
    std::ignore = host_->GetEventSink()->OnEventFromSource(&event);
  }
  void Wheel(ui::MouseWheelEvent& event) override {
    std::ignore = host_->GetEventSink()->OnEventFromSource(&event);
  }
  void Key(ui::KeyEvent& event) override {
    std::ignore = host_->GetEventSink()->OnEventFromSource(&event);
  }
  // Blink runs Control's editing shortcuts itself.
  std::optional<std::string_view> EditShortcut(
      const ui::KeyEvent& event) override {
    return std::nullopt;
  }
  void Type(char16_t character, int flags) override {
    ui::TextInputClient* client = host_->GetInputMethod()->GetTextInputClient();
    if (client) {
      client->InsertChar(ui::KeyEvent::FromCharacter(
          character, ui::VKEY_UNKNOWN, ui::DomCode::NONE, flags));
    }
  }

  // ui::InputMethodObserver:
  void OnFocus() override {}
  void OnBlur() override {}
  void OnCaretBoundsChanged(const ui::TextInputClient* client) override {}
  void OnTextInputStateChanged(const ui::TextInputClient* client) override {
    if (delegate_) {
      delegate_->TextInputMayHaveChanged();
    }
  }
  void OnInputMethodDestroyed(const ui::InputMethod* input_method) override {}

 private:
  // Both are cleared by Detach, before the contents go.
  raw_ptr<content::WebContents> contents_;
  raw_ptr<Delegate> delegate_;
  std::unique_ptr<TabHost> host_;
};

}  // namespace

std::unique_ptr<TabView> TabView::Create(content::WebContents& contents,
                                         const gfx::Size& size,
                                         Delegate& delegate) {
  return std::make_unique<AuraTabView>(contents, size, delegate);
}

std::unique_ptr<content::WebContentsViewDelegate> CreateViewDelegate(
    content::WebContents& contents) {
  return nullptr;
}

}  // namespace lanescape_browser
