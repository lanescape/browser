package stop_test

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/lanescape/browser/tools/internal/stop"
)

// The test binary, run with STOP_TEST_TOOL set, stands in for the tool: it
// starts children as the tool's commands do, prints its PID and those to
// watch once each child is ready, waits for its first child, as a command
// waits for what it runs, and exits as main does.
func TestMain(m *testing.M) {
	if os.Getenv("STOP_TEST_SETSID") != "" {
		setsid()
	}
	if mode := os.Getenv("STOP_TEST_TOOL"); mode != "" {
		tool(mode)
		return
	}
	os.Exit(m.Run())
}

// grace is the stand-in's.
const grace = time.Second

// Each child's script prints the PID to watch once its traps are set.
var modes = map[string][]struct {
	script string
	// session puts the child in a session, and group, of its own, as ninja
	// puts each of its commands in a group of its own.
	session bool
}{
	// Children that end at the signal, writing which it was to $ACK, one in
	// the tool's group, one apart, and a grandchild that puts itself in a
	// session of its own as it starts.
	"plain": {{script: acking}, {script: acking, session: true}, {script: `STOP_TEST_SETSID=1 "$STOP_TEST_EXE"; exit $?`}},
	// A child that ignores the signals, so only the grace's kill ends it.
	"stubborn": {{script: `trap "" INT TERM HUP; echo $$; exec sleep 60`}},
	// A child that ends at the signal, leaving a grandchild in a session of
	// its own, no longer the tool's descendant on macOS, that ignores it
	// and keeps starting processes.
	"orphan": {{script: `sh -c 'trap "" INT TERM HUP; echo $$; while :; do sleep 0.05; done' & wait`, session: true}},
	// A child that, at the signal, starts a grandchild that ignores it and
	// writes its PID to $LATE, then ends: the grandchild is the tool's only
	// while the tool is its subreaper.
	"late": {{script: `trap 'sh -c "$LATE_SCRIPT" & exit 0' TERM; echo $$; while :; do sleep 0.05; done`}},
	// A child that, at the signal, starts a grandchild and writes the status
	// it ends with to $ACK, then goes on: 143 if the signal reached it too,
	// 137 if only the grace's kill did.
	"started": {{script: `trap 'sleep 60 & wait $!; echo $? >>"$ACK"' TERM; echo $$; while :; do sleep 0.05; done`}},
	// Children that write each signal to $ACK, going on at SIGINT and ending
	// at SIGTERM, as GitHub sends them, one in the tool's group, one apart.
	"both": {{script: twice}, {script: twice, session: true}},
}

const acking = `trap 'echo INT >>"$ACK"; exit 0' INT; trap 'echo TERM >>"$ACK"; exit 0' TERM; echo $$; while :; do sleep 0.05; done`

const twice = `trap 'echo INT >>"$ACK"' INT; trap 'echo TERM >>"$ACK"; exit 0' TERM; echo $$; while :; do sleep 0.05; done`

// setsid, for the test binary run with STOP_TEST_SETSID set, leaves its
// parent's session, then runs acking.
func setsid() {
	if _, err := syscall.Setsid(); err != nil {
		panic(err)
	}
	if err := syscall.Exec("/bin/sh", []string{"sh", "-c", acking}, os.Environ()); err != nil {
		panic(err)
	}
}

func tool(mode string) {
	stop.Grace = grace
	stop.Init()
	switch mode {
	case "admission":
		admission()
	case "endgroup":
		endGroup()
	case "unwaited":
		unwaited(2*time.Second, false)
	case "exiting":
		unwaited(grace/2, true)
	}
	pids := []string{strconv.Itoa(os.Getpid())}
	var first *exec.Cmd
	for _, c := range modes[mode] {
		cmd := exec.Command("sh", "-c", c.script)
		if c.session {
			cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
		}
		out, err := cmd.StdoutPipe()
		if err != nil {
			panic(err)
		}
		if err := stop.Start(cmd); err != nil {
			panic(err)
		}
		line, err := bufio.NewReader(out).ReadString('\n')
		if err != nil {
			panic(err)
		}
		pids = append(pids, strconv.Itoa(cmd.Process.Pid), strings.TrimSpace(line))
		if first == nil {
			first = cmd
		} else {
			// Each command is waited for, as the tool's are.
			go cmd.Wait()
		}
	}
	fmt.Println(strings.Join(pids, " "))
	err := first.Wait()
	if stop.Signal() == 0 {
		fmt.Fprintln(os.Stderr, "the child ended without a signal:", err)
	}
	stop.Exit(1)
}

type run struct {
	shell *exec.Cmd
	// tool is the stand-in's PID; watched, the processes it started.
	tool    int
	watched []int
	sibling int
	ack     string
	late    string
	// log is the stand-in's stderr, unless it's a closed pipe.
	log   string
	began time.Time
	// done is the shell reaped.
	done bool
	// starts are the watched processes' start times, read while they were
	// known to be running, which tell them from later ones given their
	// PIDs.
	starts map[int]int64
}

// watch has the run check pid, running now, has ended once the stand-in
// has, and kill it when the test ends if it's still that process.
func (r *run) watch(t *testing.T, pid int) {
	t.Helper()
	start := stop.StartOf(pid)
	if start < 0 {
		t.Fatalf("process %d isn't running", pid)
	}
	if r.starts == nil {
		r.starts = map[int]int64{}
	}
	r.starts[pid] = start
	t.Cleanup(func() {
		if stop.StartOf(pid) == start {
			syscall.Kill(pid, syscall.SIGKILL)
		}
	})
}

// still is whether pid is the process it was when watched, still running.
func (r *run) still(t *testing.T, pid int) bool {
	t.Helper()
	return running(t, pid) && stop.StartOf(pid) == r.starts[pid]
}

// start runs the stand-in as a CI step runs the tool: under a shell, in a
// session with no terminal. A sibling in the shell's group, started first,
// isn't the tool's to signal.
func start(t *testing.T, mode string) *run {
	t.Helper()
	return startWith(t, mode, false)
}

// startWith is start, and with closed, its stderr is a pipe whose reader
// has gone by the time it's signalled.
func startWith(t *testing.T, mode string, closed bool) *run {
	t.Helper()
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	r := &run{ack: filepath.Join(t.TempDir(), "ack")}
	r.shell = exec.Command("sh", "-c", `sleep 60 & echo $!; "$0"; exit $?`, exe)
	r.late = filepath.Join(filepath.Dir(r.ack), "late")
	r.shell.Env = append(os.Environ(), "STOP_TEST_TOOL="+mode, "STOP_TEST_EXE="+exe, "ACK="+r.ack, "LATE="+r.late,
		`LATE_SCRIPT=trap "" INT TERM HUP; echo $$ >"$LATE"; exec sleep 60`)
	r.shell.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	var stderr io.ReadCloser
	if closed {
		pipe, err := r.shell.StderrPipe()
		if err != nil {
			t.Fatal(err)
		}
		stderr = pipe
	} else {
		// A file, which the shell and what it starts write to themselves.
		r.log = filepath.Join(filepath.Dir(r.ack), "log")
		f, err := os.Create(r.log)
		if err != nil {
			t.Fatal(err)
		}
		defer f.Close()
		r.shell.Stderr = f
	}
	out, err := r.shell.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := r.shell.Start(); err != nil {
		t.Fatal(err)
	}
	// The shell's group only while the shell is unreaped, and so its ID
	// still its own.
	t.Cleanup(func() {
		if !r.done {
			syscall.Kill(-r.shell.Process.Pid, syscall.SIGKILL)
		}
	})
	lines := make(chan []string, 1)
	go func() {
		rd := bufio.NewReader(out)
		var fields []string
		for i := 0; i < 2; i++ {
			line, _ := rd.ReadString('\n')
			fields = append(fields, strings.Fields(line)...)
		}
		lines <- fields
		io.Copy(io.Discard, rd)
	}()
	var fields []string
	select {
	case fields = <-lines:
	case <-time.After(30 * time.Second):
		t.Fatal("the stand-in never said it was ready")
	}
	var pids []int
	for _, f := range fields {
		pid, err := strconv.Atoi(f)
		if err != nil {
			t.Fatalf("not a PID: %q", f)
		}
		pids = append(pids, pid)
	}
	if len(pids) < 3 {
		t.Fatalf("PIDs %v", pids)
	}
	// Each is running: the stand-in printed them once its children were
	// ready, and nothing has been signalled.
	for _, pid := range pids {
		r.watch(t, pid)
	}
	r.sibling, r.tool, r.watched = pids[0], pids[1], pids[2:]
	if stderr != nil {
		stderr.Close()
	}
	return r
}

// signal sends sig to the stand-in alone, as GitHub signals a step's
// process.
func (r *run) signal(t *testing.T, sig syscall.Signal) {
	t.Helper()
	r.began = time.Now()
	if err := syscall.Kill(r.tool, sig); err != nil {
		t.Fatal(err)
	}
}

// status is the stand-in's exit status, which the shell passes on.
func (r *run) status(t *testing.T) int {
	t.Helper()
	done := make(chan error, 1)
	go func() { done <- r.shell.Wait() }()
	var err error
	select {
	case err = <-done:
		r.done = true
	case <-time.After(30 * time.Second):
		t.Fatal("the stand-in didn't exit")
	}
	var exit *exec.ExitError
	if err == nil {
		return 0
	}
	if !errors.As(err, &exit) {
		t.Fatal(err)
	}
	return exit.ExitCode()
}

func running(t *testing.T, pid int) bool {
	t.Helper()
	live, err := stop.Running(pid)
	if err != nil {
		t.Fatalf("listing processes: %v", err)
	}
	return live
}

// ended checks, once the stand-in has exited, that nothing it started is
// still running: the stand-in exits only once it has seen them end, as it
// says it did by not saying it couldn't confirm it.
func (r *run) ended(t *testing.T) {
	t.Helper()
	if r.log != "" {
		data, err := os.ReadFile(r.log)
		if err != nil {
			t.Fatal(err)
		}
		if strings.Contains(string(data), "couldn't confirm") {
			t.Fatalf("the stand-in exited at the kill's deadline:\n%s", data)
		}
	}
	for _, pid := range r.watched {
		if r.still(t, pid) {
			t.Fatalf("process %d outlived the stand-in", pid)
		}
	}
	if !r.still(t, r.sibling) {
		t.Fatal("a process the tool didn't start was signalled")
	}
}

// TestASignalReachesWhatTheToolStarted: signalled alone, the tool passes the
// signal on to what it started, in its group or not, or a grandchild that
// left its session, which each says it received, exits with the signal's
// status, and leaves alone what it didn't start.
func TestASignalReachesWhatTheToolStarted(t *testing.T) {
	for _, tc := range []struct {
		sig  syscall.Signal
		name string
	}{{syscall.SIGINT, "INT"}, {syscall.SIGTERM, "TERM"}} {
		t.Run(tc.name, func(t *testing.T) {
			r := start(t, "plain")
			r.signal(t, tc.sig)
			if got := r.status(t); got != 128+int(tc.sig) {
				t.Fatalf("exit status %d, want %d", got, 128+int(tc.sig))
			}
			r.ended(t)
			data, err := os.ReadFile(r.ack)
			if err != nil {
				t.Fatal(err)
			}
			if got := strings.Fields(string(data)); len(got) != 3 || got[0] != tc.name || got[1] != tc.name || got[2] != tc.name {
				t.Fatalf("they received %v, want %s three times", got, tc.name)
			}
		})
	}
}

// TestBothSignalsReachWhatTheToolStarted: SIGINT, then SIGTERM, as GitHub
// sends them, each reach what the tool started, and the tool exits with
// the first's status.
func TestBothSignalsReachWhatTheToolStarted(t *testing.T) {
	r := start(t, "both")
	acks := func() []string {
		data, _ := os.ReadFile(r.ack)
		if !strings.HasSuffix(string(data), "\n") {
			return nil
		}
		return strings.Fields(string(data))
	}
	r.signal(t, syscall.SIGINT)
	for len(acks()) < 2 {
		if time.Since(r.began) > grace/2 {
			t.Fatalf("the children received %v of SIGINT", acks())
		}
		time.Sleep(5 * time.Millisecond)
	}
	if err := syscall.Kill(r.tool, syscall.SIGTERM); err != nil {
		t.Fatal(err)
	}
	if got := r.status(t); got != 128+int(syscall.SIGINT) {
		t.Fatalf("exit status %d, want %d", got, 128+int(syscall.SIGINT))
	}
	r.ended(t)
	if got := strings.Join(acks(), " "); got != "INT INT TERM TERM" {
		t.Fatalf("the children received %q, want each SIGINT, then SIGTERM", got)
	}
}

// TestWhatIgnoresTheSignalIsKilled: what outlasts the grace is killed, and
// the tool exits with the signal's status.
func TestWhatIgnoresTheSignalIsKilled(t *testing.T) {
	r := start(t, "stubborn")
	r.signal(t, syscall.SIGTERM)
	if got := r.status(t); got != 128+int(syscall.SIGTERM) {
		t.Fatalf("exit status %d, want %d", got, 128+int(syscall.SIGTERM))
	}
	if waited := time.Since(r.began); waited < grace {
		t.Fatalf("exited after %v, before the grace", waited)
	}
	r.ended(t)
}

// TestWhatStartsWhileStoppingIsSignalledToo: a process started after the
// signal, which the first pass couldn't find, is sent it as soon as a scan
// finds it, before the grace's kill.
func TestWhatStartsWhileStoppingIsSignalledToo(t *testing.T) {
	r := start(t, "started")
	r.signal(t, syscall.SIGTERM)
	if got := r.status(t); got != 128+int(syscall.SIGTERM) {
		t.Fatalf("exit status %d, want %d", got, 128+int(syscall.SIGTERM))
	}
	r.ended(t)
	data, err := os.ReadFile(r.ack)
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.TrimSpace(string(data)); got != "143" {
		t.Fatalf("the late grandchild ended with %q, want 143, from SIGTERM", got)
	}
}

// TestAnOrphanIsKilledToo: a grandchild left behind when its parent ends at
// the signal, in a session of its own and starting processes as it's
// killed, is still the tool's to kill, and the tool waits for it, though
// the command it waited for has ended.
func TestAnOrphanIsKilledToo(t *testing.T) {
	r := start(t, "orphan")
	r.signal(t, syscall.SIGTERM)
	if got := r.status(t); got != 128+int(syscall.SIGTERM) {
		t.Fatalf("exit status %d, want %d", got, 128+int(syscall.SIGTERM))
	}
	if waited := time.Since(r.began); waited < grace {
		t.Fatalf("exited after %v, before the grace", waited)
	}
	r.ended(t)
}

// TestNothingStopsBeforeASignal is in-process: before a signal, nothing is
// stopping and a command starts.
func TestNothingStopsBeforeASignal(t *testing.T) {
	if err := stop.Err(); err != nil {
		t.Fatal(err)
	}
	if stop.Signal() != 0 || stop.Status() != 1 || stop.Context().Err() != nil {
		t.Fatal("stopping without a signal")
	}
	out, err := stop.Output(exec.Command("echo", "started"))
	if err != nil || strings.TrimSpace(string(out)) != "started" {
		t.Fatalf("%q, %v", out, err)
	}
}

// admission writes to its stderr, whose reader has gone, then, with a
// request in flight, signals the stand-in itself, then tries to start a
// command, make a request and sleep, printing what happened, and exits as
// main does.
func admission() {
	// Before any signal: the write fails, and the tool goes on.
	fmt.Fprintln(os.Stderr, "a write to a closed pipe")
	arrived := make(chan struct{})
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		close(arrived)
		<-r.Context().Done()
	}))
	inFlight := make(chan error, 1)
	go func() {
		_, err := stop.Get(server.Client(), server.URL)
		inFlight <- err
	}()
	<-arrived
	syscall.Kill(os.Getpid(), syscall.SIGTERM)
	deadline := time.Now().Add(10 * time.Second)
	for stop.Signal() == 0 {
		if time.Now().After(deadline) {
			fmt.Println("never signalled")
			os.Exit(3)
		}
		time.Sleep(time.Millisecond)
	}
	fmt.Println("in flight:", errors.Is(<-inFlight, context.Canceled))
	// And after it.
	fmt.Fprintln(os.Stderr, "a write to a closed pipe")
	err := stop.Run(exec.Command("true"))
	fmt.Println("start:", errors.Is(err, stop.ErrStopping))
	_, err = stop.Get(http.DefaultClient, "http://127.0.0.1:1/")
	fmt.Println("request:", errors.Is(err, context.Canceled))
	began := time.Now()
	stop.Sleep(time.Minute)
	fmt.Println("sleep:", time.Since(began) < 10*time.Second)
	stop.Exit(1)
}

// TestNothingStartsOnceStopping: once signalled, the tool ends a request in
// flight, starts no command, makes no request and cuts a sleep short; and a
// write to its closed stderr, before the signal or after, doesn't end it.
func TestNothingStartsOnceStopping(t *testing.T) {
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, exe)
	cmd.Env = append(os.Environ(), "STOP_TEST_TOOL=admission")
	// Its stderr is a pipe whose reader has gone.
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	r.Close()
	cmd.Stderr = w
	out, err := cmd.Output()
	w.Close()
	var exit *exec.ExitError
	if !errors.As(err, &exit) || exit.ExitCode() != 128+int(syscall.SIGTERM) {
		t.Fatalf("%v, want exit status %d", err, 128+int(syscall.SIGTERM))
	}
	if got := string(out); got != "in flight: true\nstart: true\nrequest: true\nsleep: true\n" {
		t.Fatalf("got %q", got)
	}
}

// TestALateOrphanIsKilledOnLinux: a grandchild started after the signal,
// whose parent then ends at once, is still the tool's, as its subreaper.
// On macOS, which has none, that's out of reach unless a scan finds it
// first, as the package says.
func TestALateOrphanIsKilledOnLinux(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("only a subreaper keeps a late orphan in reach")
	}
	r := start(t, "late")
	r.signal(t, syscall.SIGTERM)
	// Read while the grace keeps it running.
	var pid int
	for pid == 0 {
		if time.Since(r.began) > grace/2 {
			t.Fatal("the late grandchild never started")
		}
		data, _ := os.ReadFile(r.late)
		if s := strings.TrimSpace(string(data)); strings.HasSuffix(string(data), "\n") {
			pid, _ = strconv.Atoi(s)
		}
		time.Sleep(5 * time.Millisecond)
	}
	r.watch(t, pid)
	r.watched = append(r.watched, pid)
	if got := r.status(t); got != 128+int(syscall.SIGTERM) {
		t.Fatalf("exit status %d, want %d", got, 128+int(syscall.SIGTERM))
	}
	r.ended(t)
}

// TestAClosedLogDoesntEndTheStop: a write to a stderr whose reader has gone
// fails, rather than ending the tool before what it started.
func TestAClosedLogDoesntEndTheStop(t *testing.T) {
	r := startWith(t, "plain", true)
	r.signal(t, syscall.SIGTERM)
	if got := r.status(t); got != 128+int(syscall.SIGTERM) {
		t.Fatalf("exit status %d, want %d", got, 128+int(syscall.SIGTERM))
	}
	r.ended(t)
}

// endGroup starts a command leading a process group with another member,
// both ignoring the signals, signals the stand-in itself, then ends the
// command's group as RunFor does when its command ends, prints whether the
// member survived that, and exits as main does.
func endGroup() {
	cmd := exec.Command("sh", "-c", `trap "" INT TERM HUP; sleep 60 & echo $!; wait`)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	out, err := cmd.StdoutPipe()
	if err != nil {
		panic(err)
	}
	if err := stop.Start(cmd); err != nil {
		panic(err)
	}
	line, err := bufio.NewReader(out).ReadString('\n')
	if err != nil {
		panic(err)
	}
	member, _ := strconv.Atoi(strings.TrimSpace(line))
	go cmd.Wait()
	fmt.Println(member)
	syscall.Kill(os.Getpid(), syscall.SIGTERM)
	for stop.Signal() == 0 {
		time.Sleep(time.Millisecond)
	}
	stop.EndGroup(cmd.Process.Pid)
	time.Sleep(100 * time.Millisecond)
	fmt.Println("member alive after EndGroup:", syscall.Kill(member, 0) == nil)
	stop.Exit(1)
}

// TestEndGroupLeavesAStoppingToolsGroupAlone: once the tool is stopping,
// ending a command's group leaves its members to the grace, whose kill
// ends them.
func TestEndGroupLeavesAStoppingToolsGroupAlone(t *testing.T) {
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, exe)
	cmd.Env = append(os.Environ(), "STOP_TEST_TOOL=endgroup")
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	began := time.Now()
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	rd := bufio.NewReader(stdout)
	// Printed while the member runs, before the stand-in is signalled.
	line, err := rd.ReadString('\n')
	if err != nil {
		t.Fatal(err)
	}
	member, err := strconv.Atoi(strings.TrimSpace(line))
	if err != nil {
		t.Fatal(err)
	}
	r := &run{sibling: os.Getpid()}
	r.watch(t, member)
	// The sibling, which must still run, is the test itself.
	r.starts[os.Getpid()] = stop.StartOf(os.Getpid())
	r.watched = []int{member}
	rest, _ := io.ReadAll(rd)
	err = cmd.Wait()
	var exit *exec.ExitError
	if !errors.As(err, &exit) || exit.ExitCode() != 128+int(syscall.SIGTERM) {
		t.Fatalf("%v, want exit status %d", err, 128+int(syscall.SIGTERM))
	}
	if waited := time.Since(began); waited < grace {
		t.Fatalf("exited after %v, before the grace", waited)
	}
	if got := strings.TrimSpace(string(rest)); got != "member alive after EndGroup: true" {
		t.Fatalf("got %q", got)
	}
	r.ended(t)
}

// unwaited starts a command that ends at the signal, signals the stand-in
// itself, and collects the command's status only after d, as a command's
// Wait can, then exits as main does; or, with early, exits as main does at
// once, collecting the status meanwhile.
func unwaited(d time.Duration, early bool) {
	cmd := exec.Command("sleep", "60")
	if err := stop.Start(cmd); err != nil {
		panic(err)
	}
	syscall.Kill(os.Getpid(), syscall.SIGTERM)
	for stop.Signal() == 0 {
		time.Sleep(time.Millisecond)
	}
	collect := func() {
		time.Sleep(d)
		cmd.Wait()
	}
	if early {
		go collect()
	} else {
		collect()
	}
	stop.Exit(1)
}

// TestAnUncollectedStatusHoldsTheKillOnLinux: a command that has ended,
// whose status exec hasn't collected yet, is still a child of the tool's,
// so the grace's kill doesn't take its empty scans for the end until it's
// collected. On macOS, which can't ask, the scans alone decide.
func TestAnUncollectedStatusHoldsTheKillOnLinux(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("only Linux says whether the tool has any child left")
	}
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, exe)
	cmd.Env = append(os.Environ(), "STOP_TEST_TOOL=unwaited")
	began := time.Now()
	err = cmd.Run()
	var exit *exec.ExitError
	if !errors.As(err, &exit) || exit.ExitCode() != 128+int(syscall.SIGTERM) {
		t.Fatalf("%v, want exit status %d", err, 128+int(syscall.SIGTERM))
	}
	// The grace ends after a second, the status is collected after two.
	if waited := time.Since(began); waited < 2*time.Second {
		t.Fatalf("exited after %v, before the command's status was collected", waited)
	}
}

// TestAnUncollectedStatusHoldsACleanExitOnLinux: Exit, before the grace ends,
// doesn't take its empty scans for the end either while a command's status
// is uncollected.
func TestAnUncollectedStatusHoldsACleanExitOnLinux(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("only Linux says whether the tool has any child left")
	}
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, exe)
	cmd.Env = append(os.Environ(), "STOP_TEST_TOOL=exiting")
	began := time.Now()
	err = cmd.Run()
	var exit *exec.ExitError
	if !errors.As(err, &exit) || exit.ExitCode() != 128+int(syscall.SIGTERM) {
		t.Fatalf("%v, want exit status %d", err, 128+int(syscall.SIGTERM))
	}
	// The status is collected after half the grace, from the signal.
	waited := time.Since(began)
	if waited < grace/2 {
		t.Fatalf("exited after %v, before the command's status was collected", waited)
	}
	if waited >= grace {
		t.Fatalf("exited after %v, not before the grace ended", waited)
	}
}
