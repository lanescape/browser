#include "engine/recorder.h"

#include <algorithm>
#include <utility>

#include "base/check.h"
#include "base/containers/flat_map.h"
#include "base/logging.h"
#include "base/memory/read_only_shared_memory_region.h"
#include "base/system/sys_info.h"
#include "base/task/thread_pool.h"
#include "base/time/time.h"
#include "base/unguessable_token.h"
#include "components/viz/common/surfaces/frame_sink_id.h"
#include "components/viz/common/surfaces/video_capture_target.h"
#include "components/viz/host/host_frame_sink_manager.h"
#include "content/public/browser/context_factory.h"
#include "engine/browser.h"
#include "engine/tab_view.h"
#include "media/base/video_frame.h"
#include "media/base/video_types.h"
#include "media/capture/mojom/video_capture_buffer.mojom.h"
#include "media/capture/mojom/video_capture_types.mojom.h"
#include "mojo/public/cpp/bindings/remote.h"
#include "vpx/vp8cx.h"
#include "vpx/vpx_encoder.h"
#include "ui/compositor/compositor.h"

namespace lanescape_browser {

namespace {

constexpr base::TimeDelta kCapturePeriod = base::Milliseconds(33);
// Bits for a pixel of a frame, in screen content's terms; 1280x720 comes to 2 Mbit/s.
constexpr double kBitsPerPixel = 0.072;
constexpr double kFramesPerSecond = 30;

}  // namespace

// Runs on its own sequence: an encoder, with millisecond timestamps.
class Recorder::Writer {
 public:
  explicit Writer(uint64_t recording) : recording_(recording) {}

  ~Writer() {
    if (encoding_) {
      Flush();
      vpx_codec_destroy(&codec_);
    }
    DeliverRecordingEnd(recording_);
  }

  void ForceKeyFrame() { force_key_frame_ = true; }

  void Encode(media::mojom::VideoBufferHandlePtr data,
              media::mojom::VideoFrameInfoPtr info,
              mojo::PendingRemote<viz::mojom::FrameSinkVideoConsumerFrameCallbacks>
                  callbacks) {
    mojo::Remote<viz::mojom::FrameSinkVideoConsumerFrameCallbacks> done(
        std::move(callbacks));
    if (!data->is_read_only_shmem_region()) {
      done->Done();
      return;
    }
    const base::ReadOnlySharedMemoryRegion& region =
        data->get_read_only_shmem_region();
    auto [it, added] = mappings_.try_emplace(region.GetGUID());
    if (added) {
      if (mappings_.size() > 16) {
        base::ReadOnlySharedMemoryMapping kept = region.Map();
        mappings_.clear();
        it = mappings_.try_emplace(region.GetGUID(), std::move(kept)).first;
      } else {
        it->second = region.Map();
      }
    }
    if (!it->second.IsValid()) {
      mappings_.erase(region.GetGUID());
      done->Done();
      return;
    }
    const gfx::Rect& visible = info->visible_rect;
    scoped_refptr<media::VideoFrame> frame = media::VideoFrame::WrapExternalData(
        info->pixel_format, info->coded_size, visible, visible.size(),
        it->second.GetMemoryAsSpan<uint8_t>(), info->timestamp);
    if (!frame || !Prepare(visible.size())) {
      done->Done();
      return;
    }
    vpx_image_t image;
    vpx_img_wrap(&image, VPX_IMG_FMT_I420, visible.width(), visible.height(), 1,
                 const_cast<uint8_t*>(frame->visible_data(media::VideoFrame::Plane::kY)));
    image.planes[VPX_PLANE_Y] =
        const_cast<uint8_t*>(frame->visible_data(media::VideoFrame::Plane::kY));
    image.planes[VPX_PLANE_U] =
        const_cast<uint8_t*>(frame->visible_data(media::VideoFrame::Plane::kU));
    image.planes[VPX_PLANE_V] =
        const_cast<uint8_t*>(frame->visible_data(media::VideoFrame::Plane::kV));
    image.stride[VPX_PLANE_Y] = frame->stride(media::VideoFrame::Plane::kY);
    image.stride[VPX_PLANE_U] = frame->stride(media::VideoFrame::Plane::kU);
    image.stride[VPX_PLANE_V] = frame->stride(media::VideoFrame::Plane::kV);
    // Strictly increasing, or the encoder refuses the frame.
    const int64_t pts =
        std::max(last_pts_ + 1, info->timestamp.InMilliseconds());
    last_pts_ = pts;
    const vpx_codec_err_t error = vpx_codec_encode(
        &codec_, &image, pts, kCapturePeriod.InMilliseconds(),
        force_key_frame_ ? VPX_EFLAG_FORCE_KF : 0, VPX_DL_REALTIME);
    force_key_frame_ = false;
    vpx_img_free(&image);
    done->Done();
    if (error != VPX_CODEC_OK) {
      LOG(ERROR) << "VP8 encoding: " << vpx_codec_err_to_string(error);
      return;
    }
    Drain();
  }

 private:
  // An encoder for this size, the first one or one after a resize, which starts with a key frame.
  bool Prepare(const gfx::Size& size) {
    if (encoding_ && size == size_) {
      return true;
    }
    if (encoding_) {
      Flush();
      vpx_codec_destroy(&codec_);
      encoding_ = false;
    }
    vpx_codec_enc_cfg_t config;
    if (vpx_codec_enc_config_default(vpx_codec_vp8_cx(), &config, 0) !=
        VPX_CODEC_OK) {
      return false;
    }
    config.g_w = size.width();
    config.g_h = size.height();
    config.g_timebase = {1, 1000};
    config.g_threads =
        std::clamp(base::SysInfo::NumberOfProcessors() / 4, 1, 4);
    config.g_lag_in_frames = 0;
    config.g_error_resilient = 0;
    config.rc_end_usage = VPX_VBR;
    config.rc_target_bitrate = static_cast<unsigned int>(
        size.width() * size.height() * kBitsPerPixel * kFramesPerSecond / 1000);
    config.rc_min_quantizer = 2;
    config.rc_max_quantizer = 52;
    config.kf_mode = VPX_KF_AUTO;
    config.kf_max_dist = 150;
    if (vpx_codec_enc_init(&codec_, vpx_codec_vp8_cx(), &config, 0) !=
        VPX_CODEC_OK) {
      return false;
    }
    // The fastest speed, and blocks that didn't change aren't looked at: most of a browser's picture
    // stands still. Measured at 1280x720, against speed 8 with no threshold, a full animation's
    // encoding costs 16% of a core rather than 25%, and a small change's 9% rather than 17%. A higher
    // threshold saves little more and could lose faint changes to text.
    vpx_codec_control(&codec_, VP8E_SET_CPUUSED, 16);
    vpx_codec_control(&codec_, VP8E_SET_STATIC_THRESHOLD, 1);
    vpx_codec_control(&codec_, VP8E_SET_SCREEN_CONTENT_MODE, 1);
    vpx_codec_control(&codec_, VP8E_SET_NOISE_SENSITIVITY, 0);
    vpx_codec_control(&codec_, VP8E_SET_TOKEN_PARTITIONS, VP8_TWO_TOKENPARTITION);
    encoding_ = true;
    size_ = size;
    return true;
  }

  void Flush() {
    if (vpx_codec_encode(&codec_, nullptr, last_pts_ + 1, 1, 0, VPX_DL_REALTIME) ==
        VPX_CODEC_OK) {
      Drain();
    }
  }

  void Drain() {
    vpx_codec_iter_t iter = nullptr;
    while (const vpx_codec_cx_pkt_t* packet =
               vpx_codec_get_cx_data(&codec_, &iter)) {
      if (packet->kind != VPX_CODEC_CX_FRAME_PKT) {
        continue;
      }
      DeliverRecordingFrame(
          recording_,
          UNSAFE_TODO(base::span(
              static_cast<const uint8_t*>(packet->data.frame.buf),
              packet->data.frame.sz)),
          packet->data.frame.pts,
          (packet->data.frame.flags & VPX_FRAME_IS_KEY) != 0, size_.width(),
          size_.height());
    }
  }

  const uint64_t recording_;
  vpx_codec_ctx_t codec_;
  bool encoding_ = false;
  bool force_key_frame_ = false;
  gfx::Size size_;
  int64_t last_pts_ = -1;
  // The capture pool's buffers, which come round again and again.
  base::flat_map<base::UnguessableToken, base::ReadOnlySharedMemoryMapping>
      mappings_;
};

Recorder::Recorder(TabView& view, uint64_t recording)
    : view_(view),
      writer_(base::ThreadPool::CreateSequencedTaskRunner(
                  {base::TaskPriority::USER_VISIBLE,
                   base::TaskShutdownBehavior::BLOCK_SHUTDOWN}),
              recording) {
  capturer_ = content::GetContextFactory()
                  ->GetHostFrameSinkManager()
                  ->CreateVideoCapturer();
  capturer_->SetFormat(media::PIXEL_FORMAT_I420);
  capturer_->SetResolutionConstraints(gfx::Size(2, 2), gfx::Size(8192, 8192),
                                      /*use_fixed_aspect_ratio=*/false);
  capturer_->SetAutoThrottlingEnabled(false);
  capturer_->SetAnimationFpsLockIn(false, 0);
  capturer_->SetMinCapturePeriod(kCapturePeriod);
  capturer_->ChangeTarget(CaptureTargetOf(view.CaptureTarget()),
                          /*sub_capture_target_version=*/0);
  view.AddPopupOverlays(*capturer_);
  capturer_->Start(this, viz::mojom::BufferFormatPreference::kDefault);
  capturer_->RequestRefreshFrame();
}

Recorder::~Recorder() {
  view_->RemovePopupOverlays(*capturer_);
  capturer_->StopAndResetConsumer();
  capturer_.reset();
  // The sequence finishes the stream in the writer's destructor.
  writer_.Reset();
}

void Recorder::ChangeTarget(const viz::FrameSinkId& target) {
  capturer_->ChangeTarget(CaptureTargetOf(target),
                          /*sub_capture_target_version=*/0);
  capturer_->RequestRefreshFrame();
}

void Recorder::RequestKeyFrame() {
  writer_.AsyncCall(&Writer::ForceKeyFrame);
  capturer_->RequestRefreshFrame();
}

void Recorder::OnFrameCaptured(
    media::mojom::VideoBufferHandlePtr data,
    media::mojom::VideoFrameInfoPtr info,
    const gfx::Rect& content_rect,
    mojo::PendingRemote<viz::mojom::FrameSinkVideoConsumerFrameCallbacks>
        callbacks) {
  writer_.AsyncCall(&Writer::Encode)
      .WithArgs(std::move(data), std::move(info), std::move(callbacks));
}

}  // namespace lanescape_browser
