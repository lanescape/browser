#include "lanescape_browser/renderer.h"

#include <limits>
#include <utility>

#include "base/functional/bind.h"
#include "base/memory/weak_ptr.h"
#include "base/task/single_thread_task_runner.h"
#include "base/time/time.h"
#include "content/public/renderer/render_frame.h"
#include "content/public/renderer/render_frame_observer.h"
#include "lanescape_browser/lanescape_browser.mojom.h"
#include "mojo/public/cpp/bindings/associated_receiver.h"
#include "mojo/public/cpp/bindings/associated_remote.h"
#include "third_party/blink/public/common/associated_interfaces/associated_interface_provider.h"
#include "third_party/blink/public/common/associated_interfaces/associated_interface_registry.h"
#include "third_party/blink/public/mojom/frame/user_activation_notification_type.mojom-shared.h"
#include "third_party/blink/public/platform/task_type.h"
#include "third_party/blink/public/platform/web_string.h"
#include "third_party/blink/public/platform/web_text_input_type.h"
#include "third_party/blink/public/web/web_document.h"
#include "third_party/blink/public/web/web_element.h"
#include "third_party/blink/public/web/web_frame_widget.h"
#include "third_party/blink/public/web/web_input_method_controller.h"
#include "third_party/blink/public/web/web_local_frame.h"
#include "third_party/blink/public/web/web_range.h"
#include "third_party/blink/public/web/web_view.h"
#include "ui/gfx/geometry/rect.h"

namespace lanescape_browser {

namespace {

class FrameObserver final : public content::RenderFrameObserver,
                            public mojom::FrameAgent {
 public:
  explicit FrameObserver(content::RenderFrame* render_frame)
      : content::RenderFrameObserver(render_frame) {
    render_frame->GetAssociatedInterfaceRegistry()
        ->AddInterface<mojom::FrameAgent>(base::BindRepeating(
            &FrameObserver::Bind, weak_factory_.GetWeakPtr()));
    // Associated with the channel, so the browser hears this in order with the frame's own focus
    // notices and commits. A frame's broker wouldn't do: a commit replaces it, dropping whatever the
    // browser hasn't read yet.
    render_frame->GetRemoteAssociatedInterfaces()->GetInterface(&host_);
  }

  // content::RenderFrameObserver:
  void OnDestruct() override { delete this; }

  void FocusedElementChanged(const blink::WebElement& element) override {
    ScheduleReport();
  }


  // mojom::FrameAgent:
  void CheckFocus(CheckFocusCallback callback) override {
    if (TakesText() != reported_editable_) {
      ScheduleReport();
    }
    std::move(callback).Run();
  }

  void ReadComposition(ReadCompositionCallback callback) override {
    std::move(callback).Run(ReadLayout());
  }

  void ApplyIme(int32_t counter,
                base::TimeTicks deadline,
                mojom::ImeAction action,
                const std::u16string& text,
                int32_t selection_start,
                int32_t selection_end,
                const std::vector<ui::ImeTextSpan>& spans,
                ApplyImeCallback callback) override {
    // Too late: the browser has already answered for it.
    if (base::TimeTicks::Now() > deadline ||
        (counter != 0 &&
         (reports_ != counter || !IsFocused() || !TakesText()))) {
      std::move(callback).Run(false, ReadLayout());
      return;
    }
    blink::WebInputMethodController& controller = Controller();
    // Composition events run the page's handlers, which can remove this frame.
    base::WeakPtr<FrameObserver> self = weak_factory_.GetWeakPtr();
    const blink::WebString web_text = blink::WebString::FromUtf16(text);
    switch (action) {
      case mojom::ImeAction::kPreedit:
        // Empty text would remove the selection as well, with no composition to remove.
        if (!text.empty() || !controller.CompositionRange().IsEmpty()) {
          controller.SetComposition(web_text, spans, blink::WebRange(),
                                    selection_start, selection_end);
        }
        break;
      case mojom::ImeAction::kCommit:
        controller.CommitText(web_text, {}, blink::WebRange(), 0);
        break;
      case mojom::ImeAction::kFinish:
        controller.FinishComposingText(
            blink::WebInputMethodController::kDoNotKeepSelection);
        break;
      case mojom::ImeAction::kCancel:
        // Likewise.
        if (!controller.CompositionRange().IsEmpty()) {
          controller.SetComposition(blink::WebString(), {}, blink::WebRange(),
                                    0, 0);
        }
        break;
    }
    if (self) {
      std::move(callback).Run(true, ReadLayout());
    }
  }

  void RunCommand(int32_t token,
                  mojom::ClipboardCommand command,
                  base::TimeTicks deadline,
                  int32_t counter) override {
    // Too late: the browser may have moved on, so it decides whether to send it again.
    if (base::TimeTicks::Now() > deadline) {
      host_->CommandFinished(token, mojom::CommandEnd::kTooLate, false, false);
      return;
    }
    // Not the focus it was sent for.
    if (counter != 0 && reports_ != counter) {
      host_->CommandFinished(token, mojom::CommandEnd::kFocusMoved, false,
                             false);
      return;
    }
    host_->CommandStarted(token);
    blink::WebLocalFrame& frame = *render_frame()->GetWebFrame();
    const bool editable = IsFocused() && TakesText();
    const bool selection = frame.HasSelection();
    // A viewer's command is a person's, and carries the activation their own key press would.
    frame.NotifyUserActivation(
        blink::mojom::UserActivationNotificationType::kInteraction);
    base::WeakPtr<FrameObserver> self = weak_factory_.GetWeakPtr();
    switch (command) {
      case mojom::ClipboardCommand::kCopy:
        frame.ExecuteCommand(blink::WebString::FromUtf8("Copy"));
        break;
      case mojom::ClipboardCommand::kCut:
        frame.ExecuteCommand(blink::WebString::FromUtf8("Cut"));
        break;
      case mojom::ClipboardCommand::kPaste:
        if (editable) {
          frame.ExecuteCommand(blink::WebString::FromUtf8("Paste"));
        }
        break;
      case mojom::ClipboardCommand::kPastePlain:
        if (editable) {
          frame.ExecuteCommand(
              blink::WebString::FromUtf8("PasteAndMatchStyle"));
        }
        break;
    }
    // A handler can remove its own frame; the browser then hears that the frame went away.
    if (self) {
      host_->CommandFinished(token, mojom::CommandEnd::kRan, selection,
                             editable);
    }
  }

  void RunEditCommand(const std::string& command,
                      base::TimeTicks deadline,
                      RunEditCommandCallback callback) override {
    // Too late: later input may already have overtaken it.
    if (base::TimeTicks::Now() > deadline) {
      std::move(callback).Run();
      return;
    }
    // A handler can remove its own frame, and with it this callback, which then answers itself.
    render_frame()->GetWebFrame()->ExecuteCommand(
        blink::WebString::FromUtf8(command));
    std::move(callback).Run();
  }

 private:
  void Bind(mojo::PendingAssociatedReceiver<mojom::FrameAgent> receiver) {
    agent_.reset();
    agent_.Bind(std::move(receiver));
  }

  void ScheduleReport() {
    // Wraps long before anything could confuse an old counter with a new one.
    reports_ = reports_ == std::numeric_limits<int32_t>::max() ? 1 : reports_ + 1;
    if (report_scheduled_) {
      return;
    }
    report_scheduled_ = true;
    // A task, as focus is only part way through changing here, and so that a script moving focus
    // several times is one report of where it ended up.
    render_frame()
        ->GetTaskRunner(blink::TaskType::kInternalDefault)
        ->PostTask(FROM_HERE, base::BindOnce(&FrameObserver::ReportFocus,
                                             weak_factory_.GetWeakPtr()));
  }

  void ReportFocus() {
    report_scheduled_ = false;
    blink::WebLocalFrame& frame = *render_frame()->GetWebFrame();
    reported_editable_ = TakesText();
    const blink::WebElement element = frame.GetDocument().FocusedElement();
    // Reported only while it takes text: nothing else's position matters here.
    host_->FocusChanged(reports_, reported_editable_,
                        reported_editable_ && !element.IsNull()
                            ? element.BoundsInWidget()
                            : gfx::Rect());
  }

  blink::WebInputMethodController& Controller() {
    return *render_frame()->GetWebFrame()->GetInputMethodController();
  }

  mojom::ImeLayoutPtr ReadLayout() {
    blink::WebInputMethodController& controller = Controller();
    auto layout = mojom::ImeLayout::New();
    layout->composing = !controller.CompositionRange().IsEmpty();
    if (layout->composing) {
      controller.GetCompositionCharacterBounds(layout->character_bounds);
      // What Chromium gives a local input method for its candidate window: the caret it draws.
      gfx::Rect anchor;
      // A frame in its parent's process shares its parent's widget.
      render_frame()
          ->GetWebFrame()
          ->LocalRoot()
          ->FrameWidget()
          ->CalculateSelectionBounds(anchor, layout->caret);
    }
    return layout;
  }

  bool IsFocused() {
    blink::WebLocalFrame* frame = render_frame()->GetWebFrame();
    return frame->View()->FocusedFrame() == frame;
  }

  // Chromium's own answer for an input method, which runs no page script.
  bool TakesText() {
    return Controller().TextInputType() != blink::kWebTextInputTypeNone;
  }

  mojo::AssociatedRemote<mojom::FrameHost> host_;
  int32_t reports_ = 0;
  bool report_scheduled_ = false;
  bool reported_editable_ = false;
  mojo::AssociatedReceiver<mojom::FrameAgent> agent_{this};
  base::WeakPtrFactory<FrameObserver> weak_factory_{this};
};

}  // namespace

void RendererClient::RenderFrameCreated(content::RenderFrame* render_frame) {
  new FrameObserver(render_frame);  // Deletes itself in OnDestruct.
}

}  // namespace lanescape_browser
