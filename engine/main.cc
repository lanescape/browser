// This file runs before Chromium's libraries are usable, on raw argv and C strings.
#ifdef UNSAFE_BUFFERS_BUILD
#pragma allow_unsafe_buffers
#pragma allow_unsafe_libc_calls
#endif

#include <dlfcn.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include <string>
#include <utility>

#include "base/compiler_specific.h"
#include "base/time/time.h"
#include "content/public/app/content_main.h"
#include "engine/app.h"
#include "engine/browser.h"
#include "engine/lanescape_browser.h"
#include "engine/platform.h"

namespace {

// Chromium's child processes re-execute this binary with --type=.
bool IsChildProcess(int argc, const char** argv) {
  for (int i = 1; i < argc; ++i) {
    if (strncmp(argv[i], "--type=", 7) == 0) {
      return true;
    }
  }
  return false;
}

// The host sits beside the executable, in the directory Chromium finds its resources in.
std::string HostLibraryPath() {
  const std::string executable = lanescape_browser::ExecutablePath();
  const size_t slash = executable.rfind('/');
  return slash == std::string::npos
             ? std::string()
             : executable.substr(0, slash + 1) + LANESCAPE_BROWSER_HOST_LIBRARY;
}

// Loads the host. Returns a negative number to continue, or an exit code. The call crosses into a library built
// outside Chromium, which control-flow integrity can't check.
NO_SANITIZE("cfi-icall")
int LoadHost(LanescapeBrowserHost* host) {
  const std::string path = HostLibraryPath();
  void* library = dlopen(path.c_str(), RTLD_NOW | RTLD_LOCAL);
  if (!library) {
    fprintf(stderr, "lanescape-browser: cannot load the host: %s\n", dlerror());
    return 1;
  }
  auto init = reinterpret_cast<LanescapeBrowserHostInit>(
      dlsym(library, LANESCAPE_BROWSER_HOST_INIT));
  if (!init) {
    fprintf(stderr, "lanescape-browser: the host has no %s\n", LANESCAPE_BROWSER_HOST_INIT);
    return 1;
  }
  static const LanescapeBrowserEngine engine = {
      .post =
          [](const char* json, size_t length) {
            lanescape_browser::PostCommand(std::string(json, length));
          },
      .devtools_send =
          [](uint64_t session, uint64_t id, uint8_t is_input,
             const uint8_t* message, size_t length) {
            lanescape_browser::PostDevTools(session, id, is_input != 0,
                                      std::string(message, message + length));
          },
      .now_us =
          []() -> int64_t {
            return (base::TimeTicks::Now() - base::TimeTicks()).InMicroseconds();
          },
  };
  return init(&engine, host);
}

NO_SANITIZE("cfi-icall")
int NotifyExit(const LanescapeBrowserHost& host, int code) {
  return host.on_exit(code);
}

}  // namespace

int main(int argc, const char** argv) {
  if (const int stop = lanescape_browser::EarlyMain(argc, argv); stop >= 0) {
    return stop;
  }
  LanescapeBrowserHost host{};
  const bool is_browser = !IsChildProcess(argc, argv);
  if (is_browser) {
    if (const int stop = LoadHost(&host); stop >= 0) {
      return stop;
    }
    lanescape_browser::SetHost(host);
    // The first, as the host takes it (src/lib.rs). Chromium's own parsing takes `--data-dir`'s
    // value for an argument.
    for (int i = 1; i + 1 < argc; ++i) {
      if (strcmp(argv[i], "--data-dir") == 0) {
        lanescape_browser::SetDataDirectory(argv[i + 1]);
        break;
      }
    }
  }
  lanescape_browser::MainDelegate delegate;
  content::ContentMainParams params(&delegate);
  params.argc = argc;
  params.argv = argv;
  const int code = content::ContentMain(std::move(params));
  return is_browser ? NotifyExit(host, code) : code;
}
