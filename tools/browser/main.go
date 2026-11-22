// Command browser is Lanescape browser's build, CI and release tooling:
//
//	go run ./tools/browser build [--target T] [--release] [--sdk DIR] [--apple-sdk DIR]
//	    builds out/<target>/, ready to run, against the pinned libchromium
//	go run ./tools/browser package [--target T] [--sdk DIR] [--tests] [--apple-sdk DIR]
//	    packages out/<target>/ into dist/, and with --tests the test executables
//	go run ./tools/browser pin <version>
//	    pins a libchromium version in engine/libchromium.json
//	go run ./tools/browser ci checks
//	    what needs no build: go vet, gofmt and go test, rustfmt, clippy, actionlint
//	go run ./tools/browser ci install-test-packages
//	    what the suites need on a fresh Linux or macOS machine
//	go run ./tools/browser ci suites --target T [--build N]
//	    every suite against the package in dist/, unpacked
//	go run ./tools/browser ci launch --target T [--build N]
//	    the package's version and one launch, where the suites don't run
//	go run ./tools/browser release reserve --sha SHA
//	    tags a main or support/vN push with its build number, and prints it
//	go run ./tools/browser release promote --build N [--if-unstable]
//	    moves vN/latest forward to a published build
//	go run ./tools/browser release major --build N
//	    the major(s) a build number is tagged under
//	go run ./tools/browser publish
//	    uploads dist/'s packages, then the manifest, to the release bucket
//
// It works in the repository it's run from (LANESCAPE_BROWSER_REPO
// overrides that). Builds cache what they download and build in
// LANESCAPE_BROWSER_CACHE, by default ~/.cache/lanescape-browser, and write
// out/<target>/, or under LANESCAPE_BROWSER_OUT. Releasing reads
// GITHUB_TOKEN, GITHUB_REPOSITORY and GITHUB_REF, publishing BUILD_NUMBER
// and GITHUB_SHA, and both R2_ACCESS_KEY_ID and R2_SECRET_ACCESS_KEY
// (specs/0004-releases.md). None of them is ever printed.
package main

import (
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"slices"
	"strings"

	"github.com/lanescape/browser/tools/internal/build"
	"github.com/lanescape/browser/tools/internal/ci"
	"github.com/lanescape/browser/tools/internal/github"
	"github.com/lanescape/browser/tools/internal/r2"
	"github.com/lanescape/browser/tools/internal/release"
	"github.com/lanescape/browser/tools/internal/repo"
	"github.com/lanescape/browser/tools/internal/version"
)

// deps are what the commands reach outside the repository, which tests
// replace.
type deps struct {
	getenv   func(string) string
	promoter func(root string) (release.Promoter, error)
	repo     func(root string) release.Repo
}

func defaults() deps {
	return deps{
		getenv: os.Getenv,
		promoter: func(root string) (release.Promoter, error) {
			client, err := r2.FromEnvironment(root, os.Getenv)
			if err != nil {
				return release.Promoter{}, err
			}
			return release.Promoter{Repo: release.GitRepo{Dir: root}, Bucket: release.R2Bucket{R2: client},
				Sources: release.SourceChecker{API: &github.Client{}}.Check}, nil
		},
		repo: func(root string) release.Repo { return release.GitRepo{Dir: root} },
	}
}

func main() {
	root, err := repo.Root()
	if err == nil {
		err = run(root, os.Args[1:], os.Stdout, defaults())
	}
	if err != nil {
		var exit exitCode
		if errors.As(err, &exit) {
			os.Exit(int(exit))
		}
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

// exitCode ends the command with a status and nothing more to say.
type exitCode int

func (e exitCode) Error() string { return fmt.Sprintf("exit %d", int(e)) }

func usage(w io.Writer) {
	fmt.Fprintln(w, "usage: go run ./tools/browser <build|package|pin|ci|release|publish> ...; see tools/browser/main.go")
}

func run(root string, args []string, stdout io.Writer, d deps) error {
	if len(args) == 0 {
		usage(os.Stderr)
		return exitCode(2)
	}
	env, err := build.NewEnv(root)
	if err != nil {
		return err
	}
	env.Out = stdout
	targets := repo.Names(env.Targets)
	targetFlag := func(fs *flag.FlagSet, required bool) *string {
		def := "linux-amd64"
		if required {
			def = ""
		}
		return fs.String("target", def, "the target: "+strings.Join(targets, ", "))
	}
	checkTarget := func(t string) error {
		if !slices.Contains(targets, t) {
			return fmt.Errorf("--target must be one of %s", strings.Join(targets, ", "))
		}
		return nil
	}
	command, rest := args[0], args[1:]
	switch command {
	case "build":
		fs := flag.NewFlagSet("build", flag.ContinueOnError)
		target := targetFlag(fs, false)
		release := fs.Bool("release", false, "an optimised host, as releases ship")
		sdk := fs.String("sdk", "", "an extracted SDK to use instead of the pinned one")
		apple := fs.String("apple-sdk", "", "macOS: the Apple SDK to build with, instead of the pinned archive")
		if err := parse(fs, rest); err != nil {
			return err
		}
		if err := checkTarget(*target); err != nil {
			return err
		}
		return env.Build(build.BuildOptions{Target: *target, Release: *release, SDK: *sdk, AppleSDK: *apple})
	case "package":
		fs := flag.NewFlagSet("package", flag.ContinueOnError)
		target := targetFlag(fs, false)
		sdk := fs.String("sdk", "", "the extracted SDK it was built with, if not the pinned one")
		tests := fs.Bool("tests", false, "also the test executables, for the target")
		apple := fs.String("apple-sdk", "", "macOS: the Apple SDK the tests are built with")
		if err := parse(fs, rest); err != nil {
			return err
		}
		if err := checkTarget(*target); err != nil {
			return err
		}
		return env.Package(build.PackageOptions{Target: *target, SDK: *sdk, Tests: *tests, AppleSDK: *apple})
	case "pin":
		if len(rest) != 1 || strings.HasPrefix(rest[0], "-") {
			return errors.New("usage: pin <version>")
		}
		return env.Pin(rest[0], nil)
	case "ci":
		return runCI(env, rest, stdout, checkTarget, targetFlag)
	case "release":
		return runRelease(root, rest, stdout, d)
	case "publish":
		if len(rest) != 0 {
			return errors.New("usage: publish")
		}
		build, commit := d.getenv("BUILD_NUMBER"), d.getenv("GITHUB_SHA")
		if build == "" || commit == "" {
			return errors.New("BUILD_NUMBER and GITHUB_SHA must be set")
		}
		p := release.Publisher{Root: root, Sources: release.SourceChecker{API: &github.Client{Getenv: d.getenv}}.Check,
			R2: func() (*r2.Client, error) { return r2.FromEnvironment(root, d.getenv) }}
		return p.Publish(build, commit)
	case "help", "-h", "--help":
		usage(stdout)
		return nil
	}
	usage(os.Stderr)
	return exitCode(2)
}

func parse(fs *flag.FlagSet, args []string) error {
	if err := fs.Parse(args); err != nil {
		return exitCode(2)
	}
	if fs.NArg() > 0 {
		return fmt.Errorf("unexpected arguments: %s", strings.Join(fs.Args(), " "))
	}
	return nil
}

func runCI(env *build.Env, args []string, stdout io.Writer, checkTarget func(string) error, targetFlag func(*flag.FlagSet, bool) *string) error {
	c := &ci.CI{Env: env, Out: stdout}
	if len(args) == 0 {
		return errors.New("usage: ci <checks|install-test-packages|suites|launch>")
	}
	switch args[0] {
	case "checks":
		return c.Checks()
	case "install-test-packages":
		return c.InstallTestPackages()
	case "suites", "launch":
		fs := flag.NewFlagSet(args[0], flag.ContinueOnError)
		target := targetFlag(fs, true)
		build := fs.String("build", "", "the build number --version must name; development if none")
		if err := parse(fs, args[1:]); err != nil {
			return err
		}
		if err := checkTarget(*target); err != nil {
			return err
		}
		if args[0] == "suites" {
			return c.Suites(*target, *build)
		}
		return c.Launch(*target, *build)
	}
	return fmt.Errorf("unknown ci command %q", args[0])
}

func runRelease(root string, args []string, stdout io.Writer, d deps) error {
	if len(args) == 0 {
		return errors.New("usage: release <reserve|promote|major>")
	}
	fs := flag.NewFlagSet("release "+args[0], flag.ContinueOnError)
	sha := fs.String("sha", "", "reserve: the commit to tag")
	build := fs.String("build", "", "promote, major: the build number")
	ifUnstable := fs.Bool("if-unstable", false, "promote: do nothing unless the build's major is unstable")
	if err := parse(fs, args[1:]); err != nil {
		return err
	}
	switch args[0] {
	case "reserve":
		if *sha == "" {
			return errors.New("reserve requires --sha")
		}
		cargo, err := version.ReadCargo(root)
		if err != nil {
			return err
		}
		major, err := version.ReleaseMajor(cargo)
		if err != nil {
			return err
		}
		api := &github.Client{Getenv: d.getenv}
		number, err := release.Reserve(release.GitHubRemote{API: api}, d.getenv("GITHUB_REF"), major, release.Today(), *sha)
		if err != nil {
			return err
		}
		fmt.Fprintln(stdout, number)
		return nil
	case "major":
		if *build == "" {
			return errors.New("major requires --build")
		}
		group, err := release.PromotionGroup(*build, d.repo(root))
		if err != nil {
			return err
		}
		fmt.Fprintln(stdout, group)
		return nil
	case "promote":
		if *build == "" {
			return errors.New("promote requires --build")
		}
		// The bucket of .github/release.json, with the keys of the publish or
		// promote environment.
		p, err := d.promoter(root)
		if err != nil {
			return err
		}
		var result string
		if *ifUnstable {
			result, err = p.PromoteIfUnstable(*build)
		} else {
			result, err = p.Promote(*build)
		}
		var refused *release.Refused
		if errors.As(err, &refused) {
			// A newer build of this major got there first. Automatic
			// promotion expects that; a person asking for this build should
			// hear it was refused.
			if refused.Superseded && *ifUnstable {
				fmt.Fprintf(stdout, "not promoting %s: %s\n", *build, refused.Msg)
				return nil
			}
			return fmt.Errorf("promotion of %s refused: %s", *build, refused.Msg)
		}
		if err != nil {
			return err
		}
		fmt.Fprintln(stdout, map[string]string{
			"promoted": "promoted " + *build,
			"current":  *build + " is already latest",
			"stable":   "not promoting " + *build + ": its major is stable; promote it with the Promote build workflow",
		}[result])
		return nil
	}
	return fmt.Errorf("unknown release command %q", args[0])
}
