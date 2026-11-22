// A tab's view on macOS: Chromium's Cocoa views with no window, as its own headless mode has them
// (RenderWidgetHostViewMac::IsHeadless), on a display beyond every real one (platform_mac.mm).
//
// Three things differ from Aura. A <select> popup is a widget with a window and a compositor of its
// own, so it is captured on its own and composed into the tab's picture here, pixel for pixel. There is no
// window tree to hit test input through, so input goes to the widget under the pointer, or with focus,
// as Chromium's input router would send it. And the system's Edit menu, not Blink, runs Command's
// editing shortcuts.
#ifdef UNSAFE_BUFFERS_BUILD
#pragma allow_unsafe_buffers
#endif

#import <AppKit/AppKit.h>

#include <algorithm>
#include <map>
#include <memory>
#include <optional>
#include <utility>
#include <vector>

#include "base/check.h"
#include "base/containers/flat_map.h"
#include "base/functional/bind.h"
#include "base/functional/callback_helpers.h"
#include "base/memory/raw_ptr.h"
#include "base/memory/raw_ref.h"
#include "base/memory/read_only_shared_memory_region.h"
#include "base/memory/weak_ptr.h"
#include "base/no_destructor.h"
#include "base/time/time.h"
#include "base/timer/timer.h"
#include "base/unguessable_token.h"
#include "components/input/native_web_keyboard_event.h"
#include "components/viz/common/surfaces/video_capture_target.h"
#include "components/viz/host/client_frame_sink_video_capturer.h"
#include "components/viz/host/host_frame_sink_manager.h"
#include "content/public/browser/context_factory.h"
#include "content/public/browser/popup_menu.h"
#include "content/public/browser/focused_node_details.h"
#include "content/public/browser/render_frame_host.h"
#include "content/public/browser/render_process_host.h"
#include "content/public/browser/render_widget_host.h"
#include "content/public/browser/render_widget_host_observer.h"
#include "content/public/browser/render_widget_host_view.h"
#include "content/public/browser/web_contents.h"
#include "content/public/browser/web_contents_observer.h"
#include "content/public/browser/web_contents_view_delegate.h"
#include "engine/platform.h"
#include "engine/tab_view.h"
#include "media/base/video_frame.h"
#include "media/base/video_types.h"
#include "media/capture/mojom/video_capture_buffer.mojom.h"
#include "media/capture/mojom/video_capture_types.mojom.h"
#include "mojo/public/cpp/bindings/remote.h"
#include "services/viz/privileged/mojom/compositing/frame_sink_video_capture.mojom.h"
#include "third_party/blink/public/common/input/web_input_event.h"
#include "third_party/blink/public/common/input/web_mouse_event.h"
#include "third_party/blink/public/common/input/web_mouse_wheel_event.h"
#include "third_party/skia/include/core/SkBitmap.h"
#include "third_party/skia/include/core/SkColorSpace.h"
#include "ui/compositor/compositor.h"
#include "ui/events/blink/web_input_event.h"
#include "ui/events/event.h"
#include "ui/events/keycodes/dom/dom_code.h"
#include "ui/events/keycodes/keyboard_codes.h"
#include "ui/gfx/geometry/point_f.h"
#include "ui/gfx/geometry/rect.h"
#include "ui/gfx/geometry/rect_f.h"
#include "ui/gfx/mac/coordinate_conversion.h"

namespace lanescape_browser {

namespace {

class MacTabView;

// A widget that may have gone, found again by its IDs.
class WidgetRef {
 public:
  WidgetRef() = default;
  explicit WidgetRef(content::RenderWidgetHost& widget)
      : process_(widget.GetProcess()->GetDeprecatedID()),
        routing_(widget.GetRoutingID()) {}

  content::RenderWidgetHost* get() const {
    return routing_ ? content::RenderWidgetHost::FromID(process_, routing_)
                    : nullptr;
  }
  void reset() { *this = WidgetRef(); }

 private:
  int process_ = 0;
  int routing_ = 0;
};

// Every tab's view by its contents, for the WebContentsView delegate to find.
std::map<content::WebContents*, MacTabView*>& Views() {
  static base::NoDestructor<std::map<content::WebContents*, MacTabView*>>
      views;
  return *views;
}

// A <select> popup or another widget that opens over a page, captured on its own.
class Popup : public viz::mojom::FrameSinkVideoConsumer {
 public:
  Popup(MacTabView& view, content::RenderWidgetHost& widget);
  ~Popup() override;

  content::RenderWidgetHost& widget() { return *widget_; }
  // Where it is in the tab, or nothing until it is shown.
  std::optional<gfx::Rect> Bounds() const;
  const SkBitmap& picture() const { return picture_; }
  bool drawn() const { return !picture_.drawsNothing(); }
  // Whether it has been seen open, which happens once.
  bool opened() const { return opened_; }
  void set_opened() { opened_ = true; }

  // Composes it into `capturer`'s frames, as nearly as Chromium's overlays can, or takes it out.
  void ShowIn(viz::ClientFrameSinkVideoCapturer& capturer,
              const gfx::Size& tab);
  void RemoveFrom(viz::ClientFrameSinkVideoCapturer& capturer);

  // viz::mojom::FrameSinkVideoConsumer:
  void OnFrameCaptured(
      media::mojom::VideoBufferHandlePtr data,
      media::mojom::VideoFrameInfoPtr info,
      const gfx::Rect& content_rect,
      mojo::PendingRemote<viz::mojom::FrameSinkVideoConsumerFrameCallbacks>
          callbacks) override;
  void OnNewCaptureVersion(const media::CaptureVersion&) override {}
  void OnFrameWithEmptyRegionCapture() override {}
  void OnStopped() override {}
  void OnLog(const std::string& message) override {}

 private:
  const raw_ref<MacTabView> view_;
  const raw_ref<content::RenderWidgetHost> widget_;
  std::unique_ptr<viz::ClientFrameSinkVideoCapturer> capturer_;
  base::flat_map<base::UnguessableToken, base::ReadOnlySharedMemoryMapping>
      mappings_;
  SkBitmap picture_;
  bool opened_ = false;
  std::map<viz::ClientFrameSinkVideoCapturer*,
           std::unique_ptr<viz::ClientFrameSinkVideoCapturer::Overlay>>
      overlays_;
};

class MacTabView : public TabView,
                   public content::WebContentsObserver,
                   public content::RenderWidgetHostObserver {
 public:
  MacTabView(content::WebContents& contents,
             const gfx::Size& size,
             Delegate& delegate)
      : WebContentsObserver(&contents),
        delegate_(&delegate),
        size_(size),
        // Native <select> menus can't be captured and run a nested event loop. Chromium's own popups
        // are widgets, which can.
        external_popups_(contents.ForbidExternalPopupMenus()) {
    Views()[&contents] = this;
    // A native menu that somehow still asked to open returns at once.
    content::DontShowPopupMenus();
    Place();
    target_ = CurrentTarget();
  }

  ~MacTabView() override {
    Detach();
    CHECK(capturers_.empty());
  }

  void PopupCreated(content::RenderWidgetHost& widget) {
    if (!popups_.contains(&widget)) {
      widget.AddObserver(this);
      popups_[&widget] = std::make_unique<Popup>(*this, widget);
    }
  }

  // A popup has a new picture, or has moved.
  void PopupDrew(Popup& popup) {
    // Its window can open after it is shown, but never after it first draws.
    Conceal(popup.widget());
    // A popup opened by a press takes the pointer while the button is held, as Aura's capture gives
    // it, so that a drag onto an option and a release there choose it.
    if (!popup.opened()) {
      popup.set_opened();
      content::RenderWidgetHost* pressed = pressed_.get();
      if (pressed && !popups_.contains(pressed)) {
        pressed_ = WidgetRef(popup.widget());
      }
    }
    for (viz::ClientFrameSinkVideoCapturer* capturer : capturers_) {
      popup.ShowIn(*capturer, size_);
    }
    if (delegate_) {
      delegate_->PictureChanged();
    }
  }

  // TabView:
  void Detach() override {
    if (!web_contents()) {
      return;
    }
    for (auto& [widget, popup] : popups_) {
      widget->RemoveObserver(this);
    }
    popups_.clear();
    Views().erase(web_contents());
    Observe(nullptr);
    delegate_ = nullptr;
  }

  void Resize(const gfx::Size& size) override {
    size_ = size;
    Place();
  }

  viz::FrameSinkId CaptureTarget() override { return target_; }

  // Each popup's latest picture over the page's, where it is now, with where any was last time in
  // what changed. Premultiplied, so a pixel is the popup's plus what its transparency lets through.
  void Compose(Picture& picture) override {
    const gfx::Rect tab(picture.size);
    std::vector<std::pair<gfx::Rect, Popup*>> shown;
    for (auto& [widget, popup] : popups_) {
      const std::optional<gfx::Rect> bounds = popup->Bounds();
      if (bounds && popup->drawn() &&
          popup->picture().width() == bounds->width() &&
          popup->picture().height() == bounds->height()) {
        gfx::Rect visible = *bounds;
        visible.Intersect(tab);
        if (!visible.IsEmpty()) {
          shown.emplace_back(*bounds, popup.get());
        }
      }
    }
    for (const gfx::Rect& was : composed_) {
      picture.update.Union(was);
    }
    composed_.clear();
    if (shown.empty()) {
      return;
    }
    const size_t row = static_cast<size_t>(picture.size.width()) * 4;
    composition_.resize(row * picture.size.height());
    for (int y = 0; y < picture.size.height(); ++y) {
      memcpy(composition_.data() + y * row, picture.pixels + y * picture.stride,
             row);
    }
    for (const auto& [bounds, popup] : shown) {
      gfx::Rect visible = bounds;
      visible.Intersect(tab);
      const SkBitmap& source = popup->picture();
      // Both are 32-bit; the popup's red and blue swap if its order isn't the page's.
      const bool swap =
          (source.colorType() == kRGBA_8888_SkColorType) != picture.rgba;
      for (int y = visible.y(); y < visible.bottom(); ++y) {
        const uint8_t* from = static_cast<const uint8_t*>(
            source.getAddr(visible.x() - bounds.x(), y - bounds.y()));
        uint8_t* to = composition_.data() + y * row + visible.x() * 4;
        for (int x = 0; x < visible.width(); ++x, from += 4, to += 4) {
          const uint8_t c0 = swap ? from[2] : from[0];
          const uint8_t c2 = swap ? from[0] : from[2];
          const unsigned through = 255 - from[3];
          const auto over = [through](unsigned top, unsigned under) {
            return static_cast<uint8_t>(
                std::min(255u, top + (under * through + 127) / 255));
          };
          to[0] = over(c0, to[0]);
          to[1] = over(from[1], to[1]);
          to[2] = over(c2, to[2]);
          to[3] = over(from[3], to[3]);
        }
      }
      picture.update.Union(visible);
      composed_.push_back(visible);
    }
    picture.pixels = composition_.data();
    picture.stride = row;
  }

  base::TimeDelta TrailingRefresh() override { return base::Milliseconds(150); }

  void AddPopupOverlays(viz::ClientFrameSinkVideoCapturer& capturer) override {
    capturers_.push_back(&capturer);
    for (auto& [widget, popup] : popups_) {
      if (popup->drawn()) {
        popup->ShowIn(capturer, size_);
      }
    }
  }

  void RemovePopupOverlays(
      viz::ClientFrameSinkVideoCapturer& capturer) override {
    std::erase(capturers_, &capturer);
    for (auto& [widget, popup] : popups_) {
      popup->RemoveFrom(capturer);
    }
  }

  void Mouse(ui::MouseEvent& event) override {
    const gfx::PointF at(event.location());
    blink::WebMouseEvent web = ui::MakeWebMouseEvent(event);
    web.SetPositionInScreen(at + gfx::Vector2dF(DisplayOrigin().x(),
                                                 DisplayOrigin().y()));
    // A button held keeps the pointer's events where it went down, as Chromium's input router does.
    content::RenderWidgetHost* target = pressed_.get();
    if (!target) {
      target = WidgetAt(at);
    }
    if (!target) {
      return;
    }
    content::RenderWidgetHost* hovered = hovered_.get();
    if (hovered != target && event.type() == ui::EventType::kMouseMoved) {
      EnterAndLeave(hovered, *target, web, at);
    }
    hovered_ = WidgetRef(*target);
    web.SetPositionInWidget(InWidget(*target, at));
    if (event.type() == ui::EventType::kMousePressed) {
      pressed_ = WidgetRef(*target);
    } else if (event.type() == ui::EventType::kMouseReleased &&
               !(event.flags() & ~event.changed_button_flags() &
                 (ui::EF_LEFT_MOUSE_BUTTON | ui::EF_MIDDLE_MOUSE_BUTTON |
                  ui::EF_RIGHT_MOUSE_BUTTON))) {
      pressed_.reset();
    }
    target->ForwardMouseEvent(web);
  }

  void Wheel(ui::MouseWheelEvent& event) override {
    const gfx::PointF at(event.location());
    content::RenderWidgetHost* target = WidgetAt(at);
    if (!target) {
      return;
    }
    blink::WebMouseWheelEvent web = ui::MakeWebMouseWheelEvent(event);
    web.SetPositionInWidget(InWidget(*target, at));
    web.SetPositionInScreen(at + gfx::Vector2dF(DisplayOrigin().x(),
                                                 DisplayOrigin().y()));
    // A wheel's notches come with no phases, which Chromium needs to start and end a scroll. Its own
    // views add them as these do: the first notch begins, later ones continue, and a pause ends it.
    const bool continues =
        wheel_end_.IsRunning() && wheel_target_.get() == target;
    if (!continues && wheel_end_.IsRunning()) {
      wheel_end_.FireNow();
    }
    web.phase = continues ? blink::WebMouseWheelEvent::kPhaseChanged
                          : blink::WebMouseWheelEvent::kPhaseBegan;
    wheel_target_ = WidgetRef(*target);
    wheel_last_ = web;
    target->ForwardWheelEvent(web);
    wheel_end_.Start(FROM_HERE, base::Milliseconds(100),
                     base::BindOnce(&MacTabView::EndWheel,
                                    weak_factory_.GetWeakPtr()));
  }

  void Key(ui::KeyEvent& event) override {
    content::RenderWidgetHost* target = KeyTarget();
    if (!target) {
      return;
    }
    input::NativeWebKeyboardEvent web(event);
    // As macOS's own keyboard events say: Command's shortcuts are the system's, bar bold and italic.
    web.is_system_key = (web.GetModifiers() & blink::WebInputEvent::kMetaKey) &&
                        web.windows_key_code != ui::VKEY_B &&
                        web.windows_key_code != ui::VKEY_I;
    target->ForwardKeyboardEvent(web);
    const char16_t character = event.GetCharacter();
    if (event.type() == ui::EventType::kKeyPressed && character &&
        !web.is_system_key) {
      input::NativeWebKeyboardEvent typed = web;
      typed.SetType(blink::WebInputEvent::Type::kChar);
      typed.text[0] = character;
      typed.text[1] = 0;
      typed.unmodified_text[0] = event.GetUnmodifiedText();
      typed.unmodified_text[1] = 0;
      target->ForwardKeyboardEvent(typed);
    }
  }

  void Type(char16_t character, int flags) override {
    content::RenderWidgetHost* target = KeyTarget();
    if (!target) {
      return;
    }
    ui::KeyEvent key = ui::KeyEvent::FromCharacter(
        character, ui::VKEY_UNKNOWN, ui::DomCode::NONE, flags);
    input::NativeWebKeyboardEvent typed(key);
    typed.SetType(blink::WebInputEvent::Type::kChar);
    typed.text[0] = character;
    typed.text[1] = 0;
    typed.unmodified_text[0] = character;
    typed.unmodified_text[1] = 0;
    target->ForwardKeyboardEvent(typed);
  }

  // What Chromium on macOS gets from the Edit menu's key equivalents and from Cocoa's standard key
  // bindings (interpretKeyEvents:), which a key sent straight to a widget never reaches. Command with
  // V never gets here: the host drops it, as no viewer's clipboard is behind it. Blink's own key
  // bindings cover the rest, such as Option's word movement.
  std::optional<std::string_view> EditShortcut(
      const ui::KeyEvent& event) override {
    if (event.type() != ui::EventType::kKeyPressed) {
      return std::nullopt;
    }
    constexpr int kShift = ui::EF_SHIFT_DOWN;
    constexpr int kControl = ui::EF_CONTROL_DOWN;
    constexpr int kCommand = ui::EF_COMMAND_DOWN;
    struct Binding {
      ui::KeyboardCode key;
      int modifiers;
      std::string_view command;
    };
    static constexpr Binding kBindings[] = {
        // The Edit menu.
        {ui::VKEY_A, kCommand, "SelectAll"},
        {ui::VKEY_C, kCommand, "Copy"},
        {ui::VKEY_X, kCommand, "Cut"},
        {ui::VKEY_Z, kCommand, "Undo"},
        {ui::VKEY_Z, kCommand | kShift, "Redo"},
        // Cocoa's standard key bindings.
        // The line's ends on screen, which in right-to-left text are its logical end and start.
        {ui::VKEY_LEFT, kCommand, "MoveToLeftEndOfLine"},
        {ui::VKEY_LEFT, kCommand | kShift,
         "MoveToLeftEndOfLineAndModifySelection"},
        {ui::VKEY_RIGHT, kCommand, "MoveToRightEndOfLine"},
        {ui::VKEY_RIGHT, kCommand | kShift,
         "MoveToRightEndOfLineAndModifySelection"},
        {ui::VKEY_UP, kCommand, "MoveToBeginningOfDocument"},
        {ui::VKEY_UP, kCommand | kShift,
         "MoveToBeginningOfDocumentAndModifySelection"},
        {ui::VKEY_DOWN, kCommand, "MoveToEndOfDocument"},
        {ui::VKEY_DOWN, kCommand | kShift,
         "MoveToEndOfDocumentAndModifySelection"},
        {ui::VKEY_BACK, kCommand, "DeleteToBeginningOfLine"},
        {ui::VKEY_A, kControl, "MoveToBeginningOfParagraph"},
        {ui::VKEY_A, kControl | kShift,
         "MoveToBeginningOfParagraphAndModifySelection"},
        {ui::VKEY_E, kControl, "MoveToEndOfParagraph"},
        {ui::VKEY_E, kControl | kShift,
         "MoveToEndOfParagraphAndModifySelection"},
        {ui::VKEY_B, kControl, "MoveBackward"},
        {ui::VKEY_F, kControl, "MoveForward"},
        {ui::VKEY_P, kControl, "MoveUp"},
        {ui::VKEY_N, kControl, "MoveDown"},
        {ui::VKEY_D, kControl, "DeleteForward"},
        {ui::VKEY_H, kControl, "DeleteBackward"},
        {ui::VKEY_K, kControl, "DeleteToEndOfParagraph"},
        {ui::VKEY_Y, kControl, "Yank"},
        {ui::VKEY_T, kControl, "Transpose"},
    };
    const int held = event.flags() & (ui::EF_SHIFT_DOWN | ui::EF_CONTROL_DOWN |
                                      ui::EF_ALT_DOWN | ui::EF_COMMAND_DOWN);
    for (const Binding& binding : kBindings) {
      if (binding.key == event.key_code() && binding.modifiers == held) {
        return binding.command;
      }
    }
    return std::nullopt;
  }

  // content::WebContentsObserver:
  void RenderFrameHostChanged(content::RenderFrameHost* old_host,
                              content::RenderFrameHost* new_host) override {
    Update();
  }
  void RenderViewReady() override { Update(); }
  void PrimaryPageChanged(content::Page& page) override { Update(); }
  void RenderFrameCreated(content::RenderFrameHost* frame) override {
    if (!frame->GetParent()) {
      Update();
    }
  }
  void OnFocusChangedInPage(
      const content::FocusedNodeDetails& details) override {
    if (delegate_) {
      delegate_->TextInputMayHaveChanged();
    }
  }

  // content::RenderWidgetHostObserver:
  void RenderWidgetHostVisibilityChanged(content::RenderWidgetHost* widget,
                                         bool visible) override {
    if (visible) {
      Conceal(*widget);
    }
  }
  void RenderWidgetHostDestroyed(content::RenderWidgetHost* widget) override {
    widget->RemoveObserver(this);
    if (pressed_.get() == widget) {
      pressed_.reset();
    }
    // Taking it out of the captures repaints where it was.
    popups_.erase(widget);
    if (delegate_) {
      delegate_->PictureChanged();
    }
  }

 private:
  content::RenderWidgetHostView* PageView() {
    return web_contents() ? web_contents()->GetRenderWidgetHostView()
                          : nullptr;
  }

  viz::FrameSinkId CurrentTarget() {
    content::RenderWidgetHostView* view = PageView();
    return view ? view->GetRenderWidgetHost()->GetFrameSinkId()
                : viz::FrameSinkId();
  }

  // The tab's screen position and size, for its contents' view and the page's: with no window, a
  // view learns these only by being told.
  void Place() {
    const gfx::Rect bounds(DisplayOrigin(), size_);
    // The contents' view sizes the page's to fill it. Sizing the page's as well would give it a
    // second set of visual properties for one change.
    NSView* view = web_contents()->GetNativeView().GetNativeNSView();
    view.frame = gfx::ScreenRectToNSRect(bounds);
    placed_ = PageView();
    if (placed_) {
      placed_->SetWindowFrameInScreen(bounds);
    }
  }

  // A navigation can replace the page's view, which is what's captured.
  void Update() {
    if (!web_contents()) {
      return;
    }
    // A new page view learns where it is from the contents' view as it's made; telling it again on
    // each of these events would only repeat it.
    if (PageView() != placed_) {
      Place();
    }
    const viz::FrameSinkId target = CurrentTarget();
    if (target != target_) {
      target_ = target;
      if (delegate_) {
        delegate_->CaptureTargetChanged();
      }
    }
  }

  // A popup's window opens where the page put it, on the display out of sight. Making it transparent
  // to the window server and to the machine's own pointer keeps it so if it's ever moved into sight.
  // What it draws is captured by the compositor, not taken from the window.
  void Conceal(content::RenderWidgetHost& widget) {
    content::RenderWidgetHostView* view = widget.GetView();
    if (!view) {
      return;
    }
    NSWindow* window = view->GetNativeView().GetNativeNSView().window;
    if (window) {
      window.alphaValue = 0;
      window.ignoresMouseEvents = YES;
      window.hasShadow = NO;
    }
  }

  // A widget and the widgets of the frames it is in, nearest first. A popup is in none.
  std::vector<content::RenderWidgetHost*> Ancestry(
      content::RenderWidgetHost& widget) {
    std::vector<content::RenderWidgetHost*> ancestry{&widget};
    if (popups_.contains(&widget) || !web_contents()) {
      return ancestry;
    }
    content::RenderFrameHost* root = nullptr;
    web_contents()->ForEachRenderFrameHost(
        [&](content::RenderFrameHost* frame) {
          if (!root && frame->GetRenderWidgetHost() == &widget &&
              (!frame->GetParent() ||
               frame->GetParent()->GetRenderWidgetHost() != &widget)) {
            root = frame;
          }
        });
    for (content::RenderFrameHost* frame = root ? root->GetParent() : nullptr;
         frame; frame = frame->GetParent()) {
      if (frame->GetRenderWidgetHost() != ancestry.back()) {
        ancestry.push_back(frame->GetRenderWidgetHost());
      }
    }
    return ancestry;
  }

  // The pointer has moved to another widget, from one or from none. What it has left gets a leave;
  // the frames it is still in, and those it has entered, see it move, as Chromium's input router has
  // them.
  void EnterAndLeave(content::RenderWidgetHost* from,
                     content::RenderWidgetHost& to,
                     const blink::WebMouseEvent& event,
                     const gfx::PointF& at) {
    const std::vector<content::RenderWidgetHost*> was =
        from ? Ancestry(*from) : std::vector<content::RenderWidgetHost*>();
    const std::vector<content::RenderWidgetHost*> is = Ancestry(to);
    for (content::RenderWidgetHost* widget : was) {
      if (!std::ranges::contains(is, widget)) {
        blink::WebMouseEvent leave = event;
        leave.SetType(blink::WebInputEvent::Type::kMouseLeave);
        leave.SetPositionInWidget(InWidget(*widget, at));
        widget->ForwardMouseEvent(leave);
      }
    }
    for (content::RenderWidgetHost* widget : is) {
      if (widget != &to) {
        blink::WebMouseEvent move = event;
        move.SetType(blink::WebInputEvent::Type::kMouseMove);
        move.SetPositionInWidget(InWidget(*widget, at));
        widget->ForwardMouseEvent(move);
      }
    }
  }

  // The widget under a point of the tab: a popup over the page, or the frame Chromium's hit test
  // finds there.
  content::RenderWidgetHost* WidgetAt(const gfx::PointF& at) {
    for (auto& [widget, popup] : popups_) {
      const std::optional<gfx::Rect> bounds = popup->Bounds();
      if (bounds && gfx::RectF(*bounds).Contains(at)) {
        return widget;
      }
    }
    if (!web_contents()) {
      return nullptr;
    }
    if (content::RenderWidgetHost* found =
            web_contents()->FindWidgetAtPoint(at)) {
      return found;
    }
    content::RenderWidgetHostView* page = PageView();
    return page ? page->GetRenderWidgetHost() : nullptr;
  }

  // A point of the tab in a widget's own coordinates.
  gfx::PointF InWidget(content::RenderWidgetHost& widget,
                       const gfx::PointF& at) {
    if (auto it = popups_.find(&widget); it != popups_.end()) {
      const std::optional<gfx::Rect> bounds = it->second->Bounds();
      return bounds ? at - bounds->OffsetFromOrigin() : at;
    }
    content::RenderWidgetHostView* view = widget.GetView();
    return view ? view->TransformRootPointToViewCoordSpace(at) : at;
  }

  // Keys go to an open popup, which holds them as it holds the pointer, or to the focused frame.
  content::RenderWidgetHost* KeyTarget() {
    for (auto& [widget, popup] : popups_) {
      if (popup->Bounds()) {
        return widget;
      }
    }
    if (!web_contents()) {
      return nullptr;
    }
    if (content::RenderFrameHost* frame = web_contents()->GetFocusedFrame()) {
      return frame->GetRenderWidgetHost();
    }
    content::RenderWidgetHostView* page = PageView();
    return page ? page->GetRenderWidgetHost() : nullptr;
  }

  void EndWheel() {
    content::RenderWidgetHost* target = wheel_target_.get();
    wheel_target_.reset();
    if (!target) {
      return;
    }
    blink::WebMouseWheelEvent end = wheel_last_;
    end.SetType(blink::WebInputEvent::Type::kMouseWheel);
    end.phase = blink::WebMouseWheelEvent::kPhaseEnded;
    end.delta_x = 0;
    end.delta_y = 0;
    end.wheel_ticks_x = 0;
    end.wheel_ticks_y = 0;
    end.SetTimeStamp(base::TimeTicks::Now());
    target->ForwardWheelEvent(end);
  }

  raw_ptr<Delegate> delegate_;
  gfx::Size size_;
  base::ScopedClosureRunner external_popups_;
  viz::FrameSinkId target_;
  // The page view last told where it is. Only compared.
  raw_ptr<content::RenderWidgetHostView, DisableDanglingPtrDetection> placed_ =
      nullptr;
  std::vector<raw_ptr<viz::ClientFrameSinkVideoCapturer>> capturers_;
  std::map<content::RenderWidgetHost*, std::unique_ptr<Popup>> popups_;
  // The page with its popups over it, and where they were in it.
  std::vector<uint8_t> composition_;
  std::vector<gfx::Rect> composed_;
  WidgetRef pressed_;
  WidgetRef hovered_;
  WidgetRef wheel_target_;
  blink::WebMouseWheelEvent wheel_last_;
  base::OneShotTimer wheel_end_;
  base::WeakPtrFactory<MacTabView> weak_factory_{this};
};

Popup::Popup(MacTabView& view, content::RenderWidgetHost& widget)
    : view_(view), widget_(widget) {
  capturer_ = content::GetContextFactory()
                  ->GetHostFrameSinkManager()
                  ->CreateVideoCapturer();
  capturer_->SetFormat(media::PIXEL_FORMAT_ARGB);
  capturer_->SetResolutionConstraints(gfx::Size(1, 1), gfx::Size(8192, 8192),
                                      /*use_fixed_aspect_ratio=*/false);
  capturer_->SetAutoThrottlingEnabled(false);
  capturer_->SetAnimationFpsLockIn(false, 0);
  capturer_->SetMinCapturePeriod(base::Milliseconds(16));
  capturer_->ChangeTarget(viz::VideoCaptureTarget(widget.GetFrameSinkId()),
                          /*sub_capture_target_version=*/0);
  capturer_->Start(this, viz::mojom::BufferFormatPreference::kDefault);
}

Popup::~Popup() {
  // Out of sight first, so the captures repaint where it was.
  for (auto& [capturer, overlay] : overlays_) {
    overlay->SetBounds(gfx::RectF());
  }
  overlays_.clear();
  capturer_->StopAndResetConsumer();
}

std::optional<gfx::Rect> Popup::Bounds() const {
  content::RenderWidgetHostView* view = widget_->GetView();
  if (!view || !view->IsShowing()) {
    return std::nullopt;
  }
  // In screen coordinates, on the display whose origin the tab is at (MacTabView::Place).
  gfx::Rect bounds = view->GetViewBounds();
  bounds.Offset(-DisplayOrigin().x(), -DisplayOrigin().y());
  return bounds;
}

void Popup::ShowIn(viz::ClientFrameSinkVideoCapturer& capturer,
                   const gfx::Size& tab) {
  const std::optional<gfx::Rect> bounds = Bounds();
  if (!bounds || !drawn() || tab.IsEmpty()) {
    return;
  }
  std::unique_ptr<viz::ClientFrameSinkVideoCapturer::Overlay>& overlay =
      overlays_[&capturer];
  if (!overlay) {
    overlay = capturer.CreateOverlay(/*stacking_index=*/0);
  }
  // Bounds are fractions of the frame.
  const float width = tab.width();
  const float height = tab.height();
  overlay->SetImageAndBounds(
      picture_, gfx::RectF(bounds->x() / width, bounds->y() / height,
                           bounds->width() / width, bounds->height() / height));
}

void Popup::RemoveFrom(viz::ClientFrameSinkVideoCapturer& capturer) {
  if (auto it = overlays_.find(&capturer); it != overlays_.end()) {
    it->second->SetBounds(gfx::RectF());
    overlays_.erase(it);
  }
}

void Popup::OnFrameCaptured(
    media::mojom::VideoBufferHandlePtr data,
    media::mojom::VideoFrameInfoPtr info,
    const gfx::Rect& content_rect,
    mojo::PendingRemote<viz::mojom::FrameSinkVideoConsumerFrameCallbacks>
        callbacks) {
  mojo::Remote<viz::mojom::FrameSinkVideoConsumerFrameCallbacks> done(
      std::move(callbacks));
  if (!data->is_read_only_shmem_region() || content_rect.IsEmpty()) {
    done->Done();
    return;
  }
  const base::ReadOnlySharedMemoryRegion& region =
      data->get_read_only_shmem_region();
  auto [it, added] = mappings_.try_emplace(region.GetGUID());
  if (added) {
    // The pool is small, and replaced when the popup's size changes.
    if (mappings_.size() > 16) {
      base::ReadOnlySharedMemoryMapping kept = region.Map();
      mappings_.clear();
      it = mappings_.try_emplace(region.GetGUID(), std::move(kept)).first;
    } else {
      it->second = region.Map();
    }
  }
  if (!it->second.IsValid()) {
    mappings_.erase(it);
    done->Done();
    return;
  }
  const size_t stride = media::VideoFrame::RowBytes(
      media::VideoFrame::Plane::kARGB, info->pixel_format,
      info->coded_size.width());
  const SkColorType type = info->pixel_format == media::PIXEL_FORMAT_ABGR
                               ? kRGBA_8888_SkColorType
                               : kBGRA_8888_SkColorType;
  SkBitmap picture;
  CHECK(picture.tryAllocPixels(SkImageInfo::Make(
      content_rect.width(), content_rect.height(), type, kPremul_SkAlphaType,
      info->color_space.ToSkColorSpace())));
  const uint8_t* source = it->second.GetMemoryAs<uint8_t>() +
                          content_rect.y() * stride + content_rect.x() * 4;
  for (int row = 0; row < content_rect.height(); ++row) {
    memcpy(picture.getAddr(0, row), source + row * stride,
           static_cast<size_t>(content_rect.width()) * 4);
  }
  done->Done();
  picture.setImmutable();
  picture_ = std::move(picture);
  view_->PopupDrew(*this);
}

// Hears of each widget Chromium makes for a tab: a popup is told to its tab before it opens.
class ViewDelegate : public content::WebContentsViewDelegate {
 public:
  explicit ViewDelegate(content::WebContents& contents) : contents_(contents) {}

  NSObject<RenderWidgetHostViewMacDelegate>* GetDelegateForHost(
      content::RenderWidgetHost* widget,
      bool is_popup) override {
    if (is_popup) {
      if (auto it = Views().find(&*contents_); it != Views().end()) {
        it->second->PopupCreated(*widget);
      }
    }
    return nil;
  }

 private:
  const raw_ref<content::WebContents> contents_;
};

}  // namespace

std::unique_ptr<TabView> TabView::Create(content::WebContents& contents,
                                         const gfx::Size& size,
                                         Delegate& delegate) {
  return std::make_unique<MacTabView>(contents, size, delegate);
}

std::unique_ptr<content::WebContentsViewDelegate> CreateViewDelegate(
    content::WebContents& contents) {
  return std::make_unique<ViewDelegate>(contents);
}

}  // namespace lanescape_browser
