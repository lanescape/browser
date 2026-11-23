package stop

import (
	"errors"
	"os/exec"
	"syscall"
	"testing"
	"time"
)

// TestALinkNeedsAChildNoOlderThanItsParent: a child listed with a parent's
// PID that started before the process listed with it was an earlier
// process's child.
func TestALinkNeedsAChildNoOlderThanItsParent(t *testing.T) {
	parent := process{pid: 10, start: 500}
	for _, tc := range []struct {
		start  int64
		linked bool
	}{{499, false}, {500, true}, {501, true}} {
		if got := linked(parent, process{pid: 11, ppid: 10, start: tc.start}); got != tc.linked {
			t.Errorf("a child started at %d, its parent at 500: linked %v, want %v", tc.start, got, tc.linked)
		}
	}
}

// TestSendChecksTheStartTime: a process is signalled through a pidfd only
// if it's the one the scan read, not a later one given its PID.
func TestSendChecksTheStartTime(t *testing.T) {
	cmd := exec.Command("sleep", "30")
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		cmd.Process.Kill()
		cmd.Wait()
	})
	p, err := stat(cmd.Process.Pid)
	if err != nil {
		t.Fatal(err)
	}
	if err := send(process{pid: p.pid, start: p.start - 1}, syscall.SIGTERM); err == nil {
		t.Fatal("signalled a process of another start time")
	}
	if err := send(p, syscall.SIGTERM); err != nil {
		t.Fatal(err)
	}
	err = cmd.Wait()
	var exit *exec.ExitError
	if !errors.As(err, &exit) || exit.Sys().(syscall.WaitStatus).Signal() != syscall.SIGTERM {
		t.Fatalf("%v, want ended by SIGTERM", err)
	}
}

// TestALaunchingCommandIsntReaped: a command that has ended as it starts,
// before it's recorded, isn't taken for a child the tool adopted: a scan
// then leaves its status to exec.
func TestALaunchingCommandIsntReaped(t *testing.T) {
	afterLaunch = func(c *exec.Cmd) {
		deadline := time.Now().Add(10 * time.Second)
		for {
			if p, err := stat(c.Process.Pid); err == nil && p.ended {
				break
			}
			if time.Now().After(deadline) {
				return
			}
			time.Sleep(time.Millisecond)
		}
		scanMu.Lock()
		scan()
		scanMu.Unlock()
	}
	defer func() { afterLaunch = nil }()
	cmd := exec.Command("sh", "-c", "exit 7")
	if err := Start(cmd); err != nil {
		t.Fatal(err)
	}
	err := cmd.Wait()
	var exit *exec.ExitError
	if !errors.As(err, &exit) || exit.ExitCode() != 7 {
		t.Fatalf("%v, want exit status 7", err)
	}
}
