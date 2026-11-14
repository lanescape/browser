#ifndef LANESCAPE_BROWSER_RENDERER_H_
#define LANESCAPE_BROWSER_RENDERER_H_

#include "content/public/renderer/content_renderer_client.h"

namespace lanescape_browser {

class RendererClient : public content::ContentRendererClient {
 public:
  void RenderFrameCreated(content::RenderFrame* render_frame) override;
};

}  // namespace lanescape_browser

#endif  // LANESCAPE_BROWSER_RENDERER_H_
