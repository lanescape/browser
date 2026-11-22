#ifndef LANESCAPE_BROWSER_TAB_VIEW_H_
#define LANESCAPE_BROWSER_TAB_VIEW_H_

#include <memory>
#include <optional>
#include <string_view>
#include <cstddef>
#include <cstdint>

#include "base/time/time.h"
#include "components/viz/common/surfaces/frame_sink_id.h"
#include "components/viz/common/surfaces/video_capture_target.h"
#include "ui/gfx/geometry/rect.h"
#include "ui/gfx/geometry/size.h"

namespace content {
class WebContents;
class WebContentsViewDelegate;
}

namespace ui {
class KeyEvent;
class MouseEvent;
class MouseWheelEvent;
}

namespace viz {
class ClientFrameSinkVideoCapturer;
}

namespace lanescape_browser {

// Where a tab is shown, and how a viewer's input reaches its frames and popups. Each system has its
// own: Aura with no platform window on Linux (tab_view_aura.cc), Cocoa's views with no window on
// macOS (tab_view_mac.mm). Everything else about a tab is shared (tab.cc).
class TabView {
 public:
  class Delegate {
   public:
    // A frame may have started or stopped taking text.
    virtual void TextInputMayHaveChanged() = 0;
    // What CaptureTarget() names has changed, as a navigation swapping the page's view does.
    virtual void CaptureTargetChanged() = 0;
    // What Compose adds has changed, though the page may not have.
    virtual void PictureChanged() = 0;

   protected:
    virtual ~Delegate() = default;
  };

  // Shows `contents` at `size`, really shown, so that its <select> popups live.
  static std::unique_ptr<TabView> Create(content::WebContents& contents,
                                         const gfx::Size& size,
                                         Delegate& delegate);
  virtual ~TabView() = default;

  // Stops telling the delegate anything: the tab's contents are about to go.
  virtual void Detach() = 0;
  virtual void Resize(const gfx::Size& size) = 0;

  // The frame sink whose captures are the tab's picture. On Linux it is the tab's own root window, which
  // includes popups and never changes. On macOS it is the page's view; popups are separate there, with
  // compositors of their own.
  virtual viz::FrameSinkId CaptureTarget() = 0;

  // A captured frame of CaptureTarget(): 32-bit premultiplied pixels, B, G, R, A or, if `rgba`, R, G, B,
  // A, and the part that changed.
  struct Picture {
    const uint8_t* pixels;
    size_t stride;
    gfx::Size size;
    bool rgba;
    gfx::Rect update;
  };
  // Adds to `picture` what its capture can't include, exactly: on macOS, the tab's popups. What it
  // points to afterwards stays valid until the next call.
  virtual void Compose(Picture& picture) = 0;
  // How long after a captured change to capture again, in case the capturer missed a later one, or
  // zero for never. On macOS, where the capture targets the page's own frame sink, a change made
  // just after a capture can go uncaptured until something else changes, which on a still page is
  // never; a capture of an unchanged page carries no change, so this ends.
  virtual base::TimeDelta TrailingRefresh() = 0;
  // Composes the popups into what `capturer` captures, for as long as both live. Chromium places them
  // only to the nearest even pixel and may scale them to fit, which a recording, already lossy, can
  // bear, unlike the display.
  virtual void AddPopupOverlays(viz::ClientFrameSinkVideoCapturer& capturer) = 0;
  virtual void RemovePopupOverlays(
      viz::ClientFrameSinkVideoCapturer& capturer) = 0;

  // A viewer's input, in tab coordinates. Each reaches whichever frame or popup is under the pointer,
  // or has focus, as a person's would.
  virtual void Mouse(ui::MouseEvent& event) = 0;
  virtual void Wheel(ui::MouseWheelEvent& event) = 0;
  virtual void Key(ui::KeyEvent& event) = 0;
  // A character with no key behind it, which can only be typed.
  virtual void Type(char16_t character, int flags) = 0;

  // What a key press does beyond what the page makes of it, as one of Blink's editing commands, run
  // when the page leaves the key alone. On macOS the system's key bindings and Edit menu, not Blink,
  // give keys such as Command+A and Command+Left their meaning.
  virtual std::optional<std::string_view> EditShortcut(
      const ui::KeyEvent& event) = 0;
};

// What a capturer targets for a view's capture target: nothing while the view has no page yet.
inline std::optional<viz::VideoCaptureTarget> CaptureTargetOf(
    const viz::FrameSinkId& target) {
  return target.is_valid() ? std::optional(viz::VideoCaptureTarget(target))
                           : std::nullopt;
}

// The contents' view delegate, through which macOS's views say when a popup is made, or nothing.
std::unique_ptr<content::WebContentsViewDelegate> CreateViewDelegate(
    content::WebContents& contents);

}  // namespace lanescape_browser

#endif  // LANESCAPE_BROWSER_TAB_VIEW_H_
