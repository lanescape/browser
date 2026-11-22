#ifndef LANESCAPE_BROWSER_PLATFORM_H_
#define LANESCAPE_BROWSER_PLATFORM_H_

// What differs between systems outside a tab's view (tab_view.h): platform_linux.cc and
// platform_mac.mm.

#include <cstdint>
#include <optional>
#include <string>

#include "base/process/process_handle.h"
#include "base/values.h"
#include "ui/gfx/geometry/point.h"

namespace base {
class CommandLine;
}

namespace lanescape_browser {

// Runs first in main(), on its raw arguments and before Chromium's libraries are usable. On macOS a
// child process enters its Seatbelt sandbox here, before it runs anything else. Returns a negative
// number to continue, or an exit code.
int EarlyMain(int argc, const char** argv);

// The executable's own path, or empty if it can't be found.
std::string ExecutablePath();

// The browser process's switches for this system's display and GPU.
void AppendPlatformSwitches(base::CommandLine& command_line, bool cpu);

// The browser process, before its message loop exists. Returns an exit code to stop with.
std::optional<int> PreBrowserMain();

// What the host checks of a renderer's sandbox (src/lib.rs, verify_sandbox), alongside what it can
// see of the process itself.
base::DictValue RendererSandboxReport(base::ProcessId renderer);

// Where the display tabs are shown on lies, in screen coordinates. On macOS it is beyond every real
// display, so that nothing of a tab, nor any of its popups' windows, appears on the machine's screen.
gfx::Point DisplayOrigin();

// That display's ID. On macOS no real display has it, so the compositor keeps time by its own timer
// rather than by a physical screen's refresh, which can stall or stop while that screen sleeps.
int64_t DisplayId();

}  // namespace lanescape_browser

#endif  // LANESCAPE_BROWSER_PLATFORM_H_
