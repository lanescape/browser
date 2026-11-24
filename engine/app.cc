#include "engine/app.h"

#include "base/command_line.h"
#include "base/functional/bind.h"
#include "base/files/file_path.h"
#include "base/files/file_util.h"
#include "base/memory/ref_counted_memory.h"
#include "base/path_service.h"
#include "base/time/time.h"
#include "components/embedder_support/user_agent_utils.h"
#include "content/public/browser/browser_context.h"
#include "content/public/browser/devtools_manager_delegate.h"
#include "content/public/browser/navigation_handle.h"
#include "content/public/browser/navigation_throttle.h"
#include "content/public/browser/navigation_throttle_registry.h"
#include "content/public/browser/render_frame_host.h"
#include "content/public/browser/web_contents_view_delegate.h"
#include "content/public/common/content_switches.h"
#include "engine/browser.h"
#include "engine/clipboard.h"
#include "engine/lanescape_browser.mojom.h"
#include "engine/platform.h"
#include "engine/tab_view.h"
#include "mojo/public/cpp/bindings/binder_map.h"
#include "services/network/public/mojom/network_context.mojom.h"
#include "third_party/blink/public/common/associated_interfaces/associated_interface_registry.h"
#include "third_party/blink/public/mojom/clipboard/clipboard.mojom.h"
#include "ui/base/l10n/l10n_util.h"
#include "ui/base/resource/resource_bundle.h"
#include "ui/base/resource/resource_scale_factor.h"
#include "ui/gfx/image/image.h"
#include "url/gurl.h"

namespace lanescape_browser {

std::u16string ContentClient::GetLocalizedString(int message_id) {
  return l10n_util::GetStringUTF16(message_id);
}

std::string_view ContentClient::GetDataResource(
    int resource_id,
    ui::ResourceScaleFactor scale_factor) {
  return ui::ResourceBundle::GetSharedInstance().GetRawDataResourceForScale(
      resource_id, scale_factor);
}

scoped_refptr<base::RefCountedMemory> ContentClient::GetDataResourceBytes(
    int resource_id) {
  return ui::ResourceBundle::GetSharedInstance().LoadDataResourceBytes(
      resource_id);
}

std::string ContentClient::GetDataResourceString(int resource_id) {
  return ui::ResourceBundle::GetSharedInstance().LoadDataResourceString(
      resource_id);
}

gfx::Image& ContentClient::GetNativeImageNamed(int resource_id) {
  return ui::ResourceBundle::GetSharedInstance().GetNativeImageNamed(
      resource_id);
}

std::unique_ptr<content::BrowserMainParts> BrowserClient::CreateBrowserMainParts(
    bool is_integration_test) {
  return std::make_unique<BrowserMainParts>();
}

std::string BrowserClient::GetUserAgent() {
  return embedder_support::GetUserAgent();
}

blink::UserAgentMetadata BrowserClient::GetUserAgentMetadata() {
  return embedder_support::GetUserAgentMetadata();
}

std::string BrowserClient::GetProduct() {
  return embedder_support::GetProductAndVersion();
}

void BrowserClient::ConfigureNetworkContextParams(
    content::BrowserContext* context,
    bool in_memory,
    const base::FilePath& relative_partition_path,
    network::mojom::NetworkContextParams* params,
    cert_verifier::mojom::CertVerifierCreationParams* cert_params) {
  ContentBrowserClient::ConfigureNetworkContextParams(
      context, in_memory, relative_partition_path, params, cert_params);
  const base::FilePath path = context->GetPath().Append(relative_partition_path);
  params->file_paths = network::mojom::NetworkContextFilePaths::New();
  params->file_paths->data_directory = path.AppendASCII("Network");
  params->file_paths->unsandboxed_data_path = path;
  params->file_paths->cookie_database_name =
      base::FilePath(FILE_PATH_LITERAL("Cookies"));
  params->file_paths->http_cache_directory = path.AppendASCII("Cache");
  // The data directory is owner-only (specs/0002-control.md), and there is no OS keyring here.
  params->enable_encrypted_cookies = false;
  params->persist_session_cookies = true;
  params->restore_old_session_cookies = true;
}

namespace {

// Debug URLs and javascript: act before a navigation starts, so the host refuses them first. This
// is the backstop for what gets as far as a request: tabs load only web pages.
class WebPagesOnly : public content::NavigationThrottle {
 public:
  using NavigationThrottle::NavigationThrottle;

  ThrottleCheckResult WillStartRequest() override { return Check(); }
  ThrottleCheckResult WillRedirectRequest() override { return Check(); }
  const char* GetNameForLogging() override { return "WebPagesOnly"; }

 private:
  ThrottleCheckResult Check() {
    const GURL& url = navigation_handle()->GetURL();
    if (url.SchemeIsHTTPOrHTTPS() || url.SchemeIs("data") ||
        url.SchemeIs("blob") || url.SchemeIs("about")) {
      return PROCEED;
    }
    return CANCEL;
  }
};

}  // namespace

void BrowserClient::CreateThrottlesForNavigation(
    content::NavigationThrottleRegistry& registry) {
  registry.AddThrottle(std::make_unique<WebPagesOnly>(registry));
}

// Runs after Content's own, and the later binder replaces the earlier.
void BrowserClient::RegisterBrowserInterfaceBindersForFrame(
    content::RenderFrameHost* render_frame_host,
    mojo::BinderMapWithContext<content::RenderFrameHost*>* map) {
  map->Add<blink::mojom::ClipboardHost>(base::BindRepeating(&BindPageClipboard));
}

void BrowserClient::RegisterAssociatedInterfaceBindersForRenderFrameHost(
    content::RenderFrameHost& render_frame_host,
    blink::AssociatedInterfaceRegistry& associated_registry) {
  associated_registry.AddInterface<mojom::FrameHost>(
      base::BindRepeating(&BindFrameHost, render_frame_host.GetGlobalId()));
}

// Needed even with nothing to customize. Without a delegate, a command for a child session keeps
// the dispatchable the root session parsed, whose fall-through is bound to the root: it runs in
// the top frame's renderer instead of the iframe's. With one, the child parses it again itself.
std::unique_ptr<content::DevToolsManagerDelegate>
BrowserClient::CreateDevToolsManagerDelegate() {
  return std::make_unique<content::DevToolsManagerDelegate>();
}

// Every profile's storage is under the data directory. A sandboxed network service, as macOS's is,
// may reach only what is named here.
std::vector<base::FilePath> BrowserClient::GetNetworkContextsParentDirectory() {
  const base::FilePath root =
      base::MakeAbsoluteFilePath(base::FilePath(DataDirectory()));
  if (root.empty()) {
    return {};
  }
  return {root};
}

std::unique_ptr<content::WebContentsViewDelegate>
BrowserClient::GetWebContentsViewDelegate(content::WebContents* web_contents) {
  return CreateViewDelegate(*web_contents);
}

MainDelegate::MainDelegate() = default;
MainDelegate::~MainDelegate() = default;

std::optional<int> MainDelegate::BasicStartupComplete() {
  base::CommandLine* command_line = base::CommandLine::ForCurrentProcess();
  // Exact pixels need a fixed colour profile.
  command_line->AppendSwitchASCII("force-color-profile", "srgb");
  for (const char* flag : {"disable-component-update", "disable-breakpad",
                           "no-first-run", "noerrdialogs"}) {
    command_line->AppendSwitch(flag);
  }
  const bool cpu = command_line->GetSwitchValueASCII("rendering") == "cpu";
  if (cpu) {
    command_line->AppendSwitch("disable-gpu");
    command_line->AppendSwitch("disable-gpu-compositing");
  }
  AppendPlatformSwitches(*command_line, cpu);
  return std::nullopt;
}

std::optional<int> MainDelegate::PreBrowserMain() {
  if (std::optional<int> stop = ContentMainDelegate::PreBrowserMain()) {
    return stop;
  }
  return lanescape_browser::PreBrowserMain();
}

void MainDelegate::PreSandboxStartup() {
  base::FilePath assets;
  CHECK(base::PathService::Get(base::DIR_ASSETS, &assets));
  // libchromium's packs (its specs/0001-sdk.md, Runtime files): English strings, and the resources every
  // process loads, at 100%.
  ui::ResourceBundle::InitSharedInstanceWithPakPath(
      assets.AppendASCII("locales").AppendASCII("en-US.pak"));
  ui::ResourceBundle::GetSharedInstance().AddDataPackFromPath(
      assets.AppendASCII("resources.pak"), ui::k100Percent);
}

content::ContentClient* MainDelegate::CreateContentClient() {
  content_client_ = std::make_unique<ContentClient>();
  return content_client_.get();
}

content::ContentBrowserClient* MainDelegate::CreateContentBrowserClient() {
  browser_client_ = std::make_unique<BrowserClient>();
  return browser_client_.get();
}

content::ContentRendererClient* MainDelegate::CreateContentRendererClient() {
  renderer_client_ = std::make_unique<RendererClient>();
  return renderer_client_.get();
}

}  // namespace lanescape_browser
