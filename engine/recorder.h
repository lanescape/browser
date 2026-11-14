#ifndef LANESCAPE_BROWSER_RECORDER_H_
#define LANESCAPE_BROWSER_RECORDER_H_

#include <memory>

#include "base/memory/raw_ref.h"
#include "base/threading/sequence_bound.h"
#include "components/viz/host/client_frame_sink_video_capturer.h"
#include "services/viz/privileged/mojom/compositing/frame_sink_video_capture.mojom.h"

namespace viz {
class FrameSinkId;
}

namespace lanescape_browser {

class TabView;

// Records what a tab shows as VP8. It has its own capturer in I420, whose colour conversion the GPU
// does when the compositor runs there, so it costs nothing of the RFB path's frames. Encoding and
// writing run on a sequence of their own; a frame's capture buffer stays held until it's encoded,
// so a recording that falls behind drops frames rather than holding up the page.
class Recorder : public viz::mojom::FrameSinkVideoConsumer {
 public:
  // Records what `view` shows, popups included. Its frames go to DeliverRecordingFrame, then
  // DeliverRecordingEnd once it has been destroyed. The view outlives it.
  Recorder(TabView& view, uint64_t recording);
  ~Recorder() override;

  void RequestKeyFrame();
  // The view's capture target has changed.
  void ChangeTarget(const viz::FrameSinkId& target);

  // viz::mojom::FrameSinkVideoConsumer:
  void OnFrameCaptured(
      media::mojom::VideoBufferHandlePtr data,
      media::mojom::VideoFrameInfoPtr info,
      const gfx::Rect& content_rect,
      mojo::PendingRemote<viz::mojom::FrameSinkVideoConsumerFrameCallbacks>
          callbacks) override;
  void OnNewCaptureVersion(
      const media::CaptureVersion& capture_version) override {}
  void OnFrameWithEmptyRegionCapture() override {}
  void OnStopped() override {}
  void OnLog(const std::string& message) override {}

 private:
  class Writer;

  const raw_ref<TabView> view_;
  std::unique_ptr<viz::ClientFrameSinkVideoCapturer> capturer_;
  base::SequenceBound<Writer> writer_;
};

}  // namespace lanescape_browser

#endif  // LANESCAPE_BROWSER_RECORDER_H_
