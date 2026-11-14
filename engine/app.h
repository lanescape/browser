#ifndef LANESCAPE_BROWSER_APP_H_
#define LANESCAPE_BROWSER_APP_H_

#include <memory>
#include <optional>

#include "content/public/app/content_main_delegate.h"
#include "content/public/browser/content_browser_client.h"
#include "content/public/common/content_client.h"
#include "lanescape_browser/renderer.h"

namespace lanescape_browser {

class ContentClient : public content::ContentClient {
 public:
  std::u16string GetLocalizedString(int message_id) override;
  std::string_view GetDataResource(int resource_id,
                                   ui::ResourceScaleFactor scale_factor) override;
  scoped_refptr<base::RefCountedMemory> GetDataResourceBytes(
      int resource_id) override;
  std::string GetDataResourceString(int resource_id) override;
  gfx::Image& GetNativeImageNamed(int resource_id) override;
};

class BrowserClient : public content::ContentBrowserClient {
 public:
  std::unique_ptr<content::BrowserMainParts> CreateBrowserMainParts(
      bool is_integration_test) override;
  std::string GetUserAgent() override;
  blink::UserAgentMetadata GetUserAgentMetadata() override;
  std::string GetProduct() override;
  void ConfigureNetworkContextParams(
      content::BrowserContext* context,
      bool in_memory,
      const base::FilePath& relative_partition_path,
      network::mojom::NetworkContextParams* network_context_params,
      cert_verifier::mojom::CertVerifierCreationParams*
          cert_verifier_creation_params) override;
  void CreateThrottlesForNavigation(
      content::NavigationThrottleRegistry& registry) override;
  void RegisterBrowserInterfaceBindersForFrame(
      content::RenderFrameHost* render_frame_host,
      mojo::BinderMapWithContext<content::RenderFrameHost*>* map) override;
  void RegisterAssociatedInterfaceBindersForRenderFrameHost(
      content::RenderFrameHost& render_frame_host,
      blink::AssociatedInterfaceRegistry& associated_registry) override;
  std::unique_ptr<content::DevToolsManagerDelegate>
  CreateDevToolsManagerDelegate() override;
  std::vector<base::FilePath> GetNetworkContextsParentDirectory() override;
  std::unique_ptr<content::WebContentsViewDelegate> GetWebContentsViewDelegate(
      content::WebContents* web_contents) override;
};

class MainDelegate : public content::ContentMainDelegate {
 public:
  MainDelegate();
  ~MainDelegate() override;

  std::optional<int> BasicStartupComplete() override;
  std::optional<int> PreBrowserMain() override;
  void PreSandboxStartup() override;
  content::ContentClient* CreateContentClient() override;
  content::ContentBrowserClient* CreateContentBrowserClient() override;
  content::ContentRendererClient* CreateContentRendererClient() override;

 private:
  std::unique_ptr<ContentClient> content_client_;
  std::unique_ptr<BrowserClient> browser_client_;
  std::unique_ptr<RendererClient> renderer_client_;
};

}  // namespace lanescape_browser

#endif  // LANESCAPE_BROWSER_APP_H_
