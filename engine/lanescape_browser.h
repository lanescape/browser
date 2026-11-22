// The boundary between the Chromium executable and the Rust host library. Chromium links as an
// executable, as Chrome does: V8 and Blink keep thread-local state a shared library can't hold, and
// release builds' whole-program hardening expects it. The executable loads the host into the
// browser process only, so renderers and the zygote never run Rust, and a host-only change relinks
// nothing.
#ifndef LANESCAPE_BROWSER_LANESCAPE_BROWSER_H_
#define LANESCAPE_BROWSER_LANESCAPE_BROWSER_H_

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// One captured frame, valid only during the callback: the host copies what it keeps.
// Pixels are 32-bit, premultiplied, in `byte_order` order. `update_*` bounds what changed since
// the previous frame of this tab; `sequence_gap` means an earlier frame was lost, so treat the
// whole frame as changed.
typedef struct {
  uint64_t tab;
  uint32_t width;
  uint32_t height;
  uint32_t stride;
  const uint8_t* pixels;
  uint32_t update_x;
  uint32_t update_y;
  uint32_t update_width;
  uint32_t update_height;
  uint8_t sequence_gap;
  // 0: bytes are B, G, R, A. 1: bytes are R, G, B, A.
  uint8_t byte_order;
} LanescapeBrowserFrame;

// What the executable offers the host.
typedef struct {
  // Queues a UTF-8 JSON command for the UI thread. Safe from any thread once on_started has run.
  void (*post)(const char* json, size_t length);
  // Queues a Chrome DevTools Protocol message for a session, under `id`. The same thread safety,
  // and in order with `post`. An input command is refused, immediately before it would reach the
  // tab, if a viewer controls the tab by then; the host then gets a devtools_refused event.
  void (*devtools_send)(uint64_t session,
                        uint64_t id,
                        uint8_t is_input,
                        const uint8_t* message,
                        size_t length);
  // Chromium's clock, in microseconds. Any thread.
  int64_t (*now_us)(void);
} LanescapeBrowserEngine;

// What the host offers the executable.
typedef struct {
  // The browser process is ready for `post`. Runs on the UI thread, once. The host may start
  // threads from here on, and must not start any earlier: Chromium's startup requires a single
  // thread.
  void (*on_started)(void);
  // A UTF-8 JSON object describing an event. Runs on the UI thread.
  void (*on_event)(const char* json, size_t length);
  // Runs on the UI thread.
  void (*on_frame)(const LanescapeBrowserFrame* frame);
  // A Chrome DevTools Protocol message for a session. Runs on the UI thread.
  void (*on_devtools)(uint64_t session, const uint8_t* message, size_t length);
  // A recording's next frame, with its timestamp in milliseconds. Runs on the recording's own
  // sequence. Null `data` means it has sent its last.
  void (*on_recording)(uint64_t recording,
                       const uint8_t* data,
                       size_t length,
                       int64_t pts,
                       uint8_t key,
                       uint32_t width,
                       uint32_t height);
  // Chromium has shut down with this exit code. Returns the process's exit code.
  int (*on_exit)(int code);
} LanescapeBrowserHost;

// Exported by the host library, and called once before Chromium starts, on the only thread. Fills
// `host`. Returns a negative number to continue, or the exit code to stop before Chromium starts.
typedef int (*LanescapeBrowserHostInit)(const LanescapeBrowserEngine* engine,
                                        LanescapeBrowserHost* host);

#define LANESCAPE_BROWSER_HOST_INIT "lanescape_browser_host_init"
#if defined(__APPLE__)
#define LANESCAPE_BROWSER_HOST_LIBRARY "liblanescape_browser_host.dylib"
#else
#define LANESCAPE_BROWSER_HOST_LIBRARY "liblanescape_browser_host.so"
#endif

#ifdef __cplusplus
}
#endif

#endif  // LANESCAPE_BROWSER_LANESCAPE_BROWSER_H_
