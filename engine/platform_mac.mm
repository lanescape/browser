// macOS: Cocoa without a window on screen, Metal through ANGLE, and Seatbelt.
#ifdef UNSAFE_BUFFERS_BUILD
#pragma allow_unsafe_buffers
#pragma allow_unsafe_libc_calls
#endif

#import <AppKit/AppKit.h>
#include <mach-o/dyld.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include <vector>

#include "base/check.h"
#include "base/command_line.h"
#include "base/mac/scoped_sending_event.h"
#include "base/message_loop/message_pump_apple.h"
#include "base/observer_list.h"
#include "content/public/browser/native_event_processor_mac.h"
#include "content/public/browser/native_event_processor_observer_mac.h"
#include "engine/platform.h"
#include "sandbox/mac/seatbelt_exec.h"

// Not in the SDK's headers, but the documented way to ask whether a process is sandboxed, and what
// its sandbox allows: 1 means sandboxed, or the operation denied. Chromium declares it the same way
// (sandbox/mac/seatbelt.cc).
extern "C" int sandbox_check(pid_t pid, const char* operation, int type, ...);

// Chromium's message pump and event handling expect an application that says when it is sending an
// event, and lets Content watch its events for responsiveness. Lanescape browser's receives none from a
// person: nothing of it is on screen.
@interface LanescapeBrowserApplication
    : NSApplication <CrAppProtocol, CrAppControlProtocol, NativeEventProcessor>
@end

@implementation LanescapeBrowserApplication {
  BOOL _handlingSendEvent;
  base::ObserverList<content::NativeEventProcessorObserver>::Unchecked
      _observers;
}

- (void)addNativeEventProcessorObserver:
    (content::NativeEventProcessorObserver*)observer {
  _observers.AddObserver(observer);
}

- (void)removeNativeEventProcessorObserver:
    (content::NativeEventProcessorObserver*)observer {
  _observers.RemoveObserver(observer);
}

- (BOOL)isHandlingSendEvent {
  return _handlingSendEvent;
}

- (void)setHandlingSendEvent:(BOOL)handlingSendEvent {
  _handlingSendEvent = handlingSendEvent;
}

- (void)sendEvent:(NSEvent*)event {
  base::mac::ScopedSendingEvent sending;
  content::ScopedNotifyNativeEventProcessorObserver notify(&_observers, event);
  [super sendEvent:event];
}

@end

namespace lanescape_browser {

int EarlyMain(int argc, const char** argv) {
  // A child process launched sandboxed is passed the policy through a pipe, and must apply it before
  // doing anything else. Chromium checks it did once its main starts.
  sandbox::SeatbeltExecServer::CreateFromArgumentsResult seatbelt =
      sandbox::SeatbeltExecServer::CreateFromArguments(argv[0], argc, argv);
  if (seatbelt.sandbox_required &&
      !(seatbelt.server && seatbelt.server->InitializeSandbox())) {
    fprintf(stderr, "lanescape-browser: cannot enter the sandbox\n");
    return 1;
  }
  return -1;
}

std::string ExecutablePath() {
  uint32_t size = 0;
  _NSGetExecutablePath(nullptr, &size);
  std::vector<char> path(size + 1);
  if (_NSGetExecutablePath(path.data(), &size) != 0) {
    return {};
  }
  // The path it was run by can go through symbolic links; the host is beside the real file.
  char resolved[PATH_MAX];
  return realpath(path.data(), resolved) ? std::string(resolved)
                                         : std::string();
}

void AppendPlatformSwitches(base::CommandLine& command_line, bool cpu) {
  // On the CPU a page has no WebGL. On macOS, disabling the GPU leaves WebGL on Metal all the same,
  // with SwiftShader as its fallback, so both go too, and so does rasterizing on the GPU.
  if (cpu) {
    command_line.AppendSwitch("disable-3d-apis");
    command_line.AppendSwitch("disable-gpu-rasterization");
    command_line.AppendSwitch("disable-software-rasterizer");
  }
  if (!cpu) {
    command_line.AppendSwitchASCII("use-gl", "angle");
    command_line.AppendSwitchASCII("use-angle", "metal");
  }
}

std::optional<int> PreBrowserMain() {
  // A process that is already sandboxed can't sandbox its renderers: macOS refuses to apply a second
  // sandbox. Stop now, rather than when the first page would run.
  if (sandbox_check(getpid(), nullptr, 0) == 1) {
    fprintf(stderr,
            "lanescape-browser: No usable sandbox: this process is already "
            "sandboxed, so its renderers can't be\n");
    return 1;
  }
  [LanescapeBrowserApplication sharedApplication];
  CHECK([NSApp isKindOfClass:[LanescapeBrowserApplication class]]);
  // No Dock icon, menu bar or activation: nothing of Lanescape browser is shown on this machine.
  [NSApp setActivationPolicy:NSApplicationActivationPolicyProhibited];
  // Without a window on screen, macOS would otherwise throttle the process's timers as an idle
  // background application's. The activity lasts as long as its token, which is kept for good.
  static CFTypeRef activity = CFBridgingRetain([NSProcessInfo.processInfo
      beginActivityWithOptions:(NSActivityUserInitiatedAllowingIdleSystemSleep |
                                NSActivityLatencyCritical) &
                               ~(NSActivitySuddenTerminationDisabled |
                                 NSActivityAutomaticTerminationDisabled)
                        reason:@"Lanescape browser serves tabs to remote viewers"]);
  CHECK(activity);
  return std::nullopt;
}

base::DictValue RendererSandboxReport(base::ProcessId renderer) {
  // The host checks the process itself too; this is Chromium's side of the same question.
  return base::DictValue()
      .Set("status", renderer ? sandbox_check(renderer, nullptr, 0) : 0)
      .Set("renderer_pid", static_cast<int>(renderer));
}

gfx::Point DisplayOrigin() {
  // Far to the left of any arrangement of real displays. A popup's window opens where its page
  // places it, inside this display, so it opens out of sight too.
  return gfx::Point(-100000, 0);
}

int64_t DisplayId() {
  // Display IDs are 32-bit on macOS, and viz takes one beyond that range for no display: real ones
  // count up from 1, and the first is the main screen.
  return int64_t{1} << 40;
}

}  // namespace lanescape_browser
