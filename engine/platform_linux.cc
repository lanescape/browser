// Linux: headless Ozone, Vulkan through ANGLE, and the zygote's sandbox.
#ifdef UNSAFE_BUFFERS_BUILD
#pragma allow_unsafe_buffers
#endif

#include <unistd.h>

#include "base/command_line.h"
#include "content/public/browser/zygote_host/zygote_host_linux.h"
#include "engine/platform.h"

namespace lanescape_browser {

int EarlyMain(int argc, const char** argv) {
  return -1;
}

std::string ExecutablePath() {
  char path[4096];
  const ssize_t length = readlink("/proc/self/exe", path, sizeof(path) - 1);
  return length > 0 ? std::string(path, static_cast<size_t>(length))
                    : std::string();
}

void AppendPlatformSwitches(base::CommandLine& command_line, bool cpu) {
  // Lanescape browser has no display server.
  command_line.AppendSwitchASCII("ozone-platform", "headless");
  if (!cpu) {
    command_line.AppendSwitchASCII("use-gl", "angle");
    command_line.AppendSwitchASCII("use-angle", "vulkan");
    command_line.AppendSwitchASCII("enable-features",
                                   "Vulkan,VulkanFromANGLE,DefaultANGLEVulkan");
  }
}

std::optional<int> PreBrowserMain() {
  return std::nullopt;
}

base::DictValue RendererSandboxReport(base::ProcessId renderer) {
  return base::DictValue()
      .Set("status",
           content::ZygoteHost::GetInstance()->GetRendererSandboxStatus())
      .Set("renderer_pid", static_cast<int>(renderer));
}

gfx::Point DisplayOrigin() {
  return gfx::Point();
}

int64_t DisplayId() {
  return 1;
}

}  // namespace lanescape_browser
