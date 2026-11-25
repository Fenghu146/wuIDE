// wulease coordinates threads that touch the same paths.
//
// It is a Go port of script/lease and is meant to run beside it: the lease
// file format is byte-compatible, so both implementations read and write the
// same files under the shared .git/amp-leases directory and each one sees the
// other's leases.
//
// Storage is one file per lease, <token>.lease, holding key=value lines. That
// is line-oriented on purpose: it stays readable by sed and by people, needs
// no jq, and keeps notes with spaces or quotes intact.
//
// Every lease file is read once into a struct, so a claim costs no process per
// lease. The only subprocess left is the one git call that finds the shared git
// directory.
package main

import (
	"fmt"
	"io"
	"math/rand/v2"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"
)

const (
	kindWork  = "work"
	kindBuild = "build"

	defaultWorkTTL  = int64(7200)
	defaultBuildTTL = int64(14400)

	claimTimeout  = 100
	claimInterval = 100 * time.Millisecond
	staleAfter    = 2 * time.Minute
)

var (
	leaseDir string
	lockDir  string
	nowUnix  int64
	threadID string
	ownToken string
	lockHeld bool
)

// lease is one parsed <token>.lease file. The timestamps keep their raw text
// so JSON output matches the shell version byte for byte, including the odd
// file that holds something that is not a number.
type lease struct {
	token      string
	thread     string
	kind       string
	note       string
	createdRaw string
	expiresRaw string
	paths      []string
}

func (l *lease) createdAt() int64 { return atoiOr(l.createdRaw) }
func (l *lease) expiresAt() int64 { return atoiOr(l.expiresRaw) }

func atoiOr(s string) int64 {
	n, err := strconv.ParseInt(strings.TrimSpace(s), 10, 64)
	if err != nil {
		return 0
	}
	return n
}

// numOrZero is ${L_X:-0}: an empty field reads as zero, anything else is
// passed through untouched.
func numOrZero(s string) string {
	if s == "" {
		return "0"
	}
	return s
}

func main() {
	nowUnix = time.Now().Unix()
	threadID = os.Getenv("AMP_THREAD_ID")
	ownToken = os.Getenv("AMP_LEASE_TOKEN")

	if err := resolveLeaseDir(); err != nil {
		die("%s", err)
	}
	defer releaseLock()

	command := "help"
	if len(os.Args) > 1 {
		command = os.Args[1]
	}
	args := os.Args[2:]

	switch command {
	case "help", "-h", "--help":
		help(os.Stdout)
	case "claim":
		cmdClaim(args)
	case "release":
		cmdRelease(args)
	case "heartbeat":
		cmdHeartbeat(args)
	case "status":
		cmdStatus(args)
	case "build":
		cmdBuild(args)
	case "check":
		cmdCheck(args)
	default:
		help(os.Stderr)
		die("unknown command: %s", command)
	}
}

func help(w io.Writer) {
	fmt.Fprint(w, `Usage: script/lease <command> [args]

  claim <path>... [--note <text>] [--ttl <time>] [--json]
      Register these paths for this thread. Exits non-zero if an active lease
      of another thread overlaps them, listing the holder, its note and the
      time left. --ttl takes 30m, 2h, 1d or plain seconds (default 2h).

  release <token>       Drop one lease.
  release --all-mine    Drop every lease of this thread.
  heartbeat <token> [--ttl <time>]
      Extend one lease. Long tasks should heartbeat instead of claiming a
      large ttl up front, so a dead thread's paths free up quickly.
  status [--json]       List active leases; also drops the expired ones.

  build claim [--note <text>] [--ttl <time>] [--json]
  build release         Hold or drop the single machine-wide build lock.
      release-fast builds and packaging take it, so two of them never share a
      target dir. Only another build claim conflicts with it.

  check --staged | --paths <path>... | --range <rev-range> [--token <token>]
      Internal, used by .githooks/pre-commit and script/pre-merge-check:
      report overlaps between these files and other threads' leases.
  check --build
      Internal: exit non-zero while the build lock is held.

Paths are compared after being reduced to a directory prefix:

  crates/code_runner/**          -> crates/code_runner/
  crates/settings_ui/src/*.rs    -> crates/settings_ui/src/
  crates/settings_ui/src/a.rs    -> crates/settings_ui/src/
  docs/x.md                      -> docs/
  **/*.rs                        -> ./  (the whole repository)

Two leases overlap when one prefix contains the other, so a claim on a file
reserves its whole directory. This is a conservative approximation: adjacent
files in one directory report a conflict even though they could be edited in
parallel. It over-reports; it does not miss a collision.

Identity comes from AMP_THREAD_ID. `+"`claim`"+` prints its token, and exporting it
as AMP_LEASE_TOKEN lets heartbeat, release and the pre-commit hook tell your
leases from everyone else's.
`)
}

// die reports and exits, dropping the claim lock first the way the shell
// version's EXIT trap does.
func die(format string, a ...any) {
	releaseLock()
	fmt.Fprintf(os.Stderr, format+"\n", a...)
	os.Exit(1)
}

func exit(code int) {
	releaseLock()
	os.Exit(code)
}

// resolveLeaseDir finds the shared git dir and the lease directory beside it.
//
// It reads the repository layout itself instead of running `git rev-parse
// --git-common-dir`, which cost about 0.2s of process start on Windows and was
// most of what a status or claim spent. The rules are the ones git documents:
// walk up from the working directory, first .git wins; a directory is the git
// dir, a file names it, and a linked worktree's git dir has a commondir file
// naming the git dir every worktree shares.
func resolveLeaseDir() error {
	gitDir, err := findCommonGitDir()
	if err != nil {
		return err
	}
	leaseDir = gitDir + "/amp-leases"
	lockDir = leaseDir + "/.claim-lock"
	return nil
}

func findCommonGitDir() (string, error) {
	cwd, err := os.Getwd()
	if err != nil {
		return "", err
	}
	for dir := cwd; ; {
		entry := filepath.Join(dir, ".git")
		info, statErr := os.Stat(entry)
		switch {
		case statErr == nil && info.IsDir():
			return physicalPath(entry)
		case statErr == nil:
			named, err := readGitDirFile(entry, dir)
			if err != nil {
				return "", err
			}
			return readCommonDir(named)
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return "", fmt.Errorf("not in a git repository: no .git in %s or any parent directory", cwd)
		}
		dir = parent
	}
}

// readGitDirFile reads a linked worktree's .git file, which holds a single
// "gitdir: <path>" line. A relative path is relative to the directory holding
// the file.
func readGitDirFile(path, dir string) (string, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return "", fmt.Errorf("cannot read %s: %v", path, err)
	}
	text := strings.TrimSpace(string(data))
	const prefix = "gitdir:"
	if len(text) < len(prefix) || !strings.EqualFold(text[:len(prefix)], prefix) {
		return "", fmt.Errorf("%s is not a git worktree file", path)
	}
	target := strings.TrimSpace(text[len(prefix):])
	if target == "" {
		return "", fmt.Errorf("%s names no git directory", path)
	}
	if !isAbsolute(target) {
		target = filepath.Join(dir, target)
	}
	return target, nil
}

// readCommonDir resolves a git dir to the one all worktrees share. A linked
// worktree's own git dir holds that name in a commondir file.
func readCommonDir(gitDir string) (string, error) {
	if data, err := os.ReadFile(filepath.Join(gitDir, "commondir")); err == nil {
		value := strings.TrimSpace(string(data))
		if value != "" {
			if !isAbsolute(value) {
				value = filepath.Join(gitDir, value)
			}
			gitDir = value
		}
	}
	return physicalPath(gitDir)
}

// isAbsolute mirrors the shell test [[ "$p" == /* || "$p" == [A-Za-z]:* ]].
func isAbsolute(p string) bool {
	if strings.HasPrefix(p, "/") {
		return true
	}
	if len(p) >= 2 && p[1] == ':' {
		c := p[0]
		return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
	}
	return false
}

// physicalPath is the shell's `cd "$dir" && pwd -P`, with forward slashes so a
// path we print looks like the one git prints.
func physicalPath(p string) (string, error) {
	abs, err := filepath.Abs(p)
	if err != nil {
		return "", err
	}
	if resolved, err := filepath.EvalSymlinks(abs); err == nil {
		abs = resolved
	}
	return filepath.ToSlash(filepath.Clean(abs)), nil
}

// normalize turns a path or glob into a directory prefix ending in /, or ./ for
// the repository root. A plain path that is not marked as a directory may be a
// file, so it reserves its parent directory too: over-reporting beats missing
// one.
func normalize(raw string) string {
	raw = strings.ReplaceAll(raw, "\\", "/")
	raw = strings.TrimPrefix(raw, "./")
	isDir := strings.HasSuffix(raw, "/")
	raw = strings.TrimSuffix(raw, "/")
	if raw == "" || raw == "." {
		return "./"
	}

	parts := strings.Split(raw, "/")
	var b strings.Builder
	count := 0
	for _, seg := range parts {
		if strings.ContainsAny(seg, "*?[") {
			break
		}
		b.WriteString(seg)
		b.WriteString("/")
		count++
	}
	prefix := b.String()

	if count == len(parts) && !isDir {
		// ${prefix%*/} then ${prefix%/*}/
		prefix = strings.TrimSuffix(prefix, "/")
		if i := strings.LastIndex(prefix, "/"); i >= 0 {
			prefix = prefix[:i] + "/"
		} else {
			prefix = prefix + "/"
		}
		if prefix == "" || prefix == "/" {
			prefix = "./"
		}
	}
	if prefix == "" {
		return "./"
	}
	return prefix
}

// overlaps reports whether one prefix contains the other. Prefixes end in /, so
// sibling directories do not collide.
func overlaps(a, b string) bool {
	if a == "./" || b == "./" {
		return true
	}
	return strings.HasPrefix(a, b) || strings.HasPrefix(b, a)
}

func humanize(s int64) string {
	if s < 0 {
		s = 0
	}
	if s < 60 {
		return strconv.FormatInt(s, 10) + "s"
	}
	m := s / 60
	if m < 60 {
		return strconv.FormatInt(m, 10) + "m"
	}
	h := m / 60
	m = m % 60
	if h < 24 {
		return fmt.Sprintf("%dh%02dm", h, m)
	}
	d := h / 24
	h = h % 24
	return fmt.Sprintf("%dd%02dh", d, h)
}

var ttlPattern = regexp.MustCompile(`^([0-9]+)([smhd]?)$`)

func parseTTL(text string) int64 {
	m := ttlPattern.FindStringSubmatch(text)
	if m == nil {
		die("invalid --ttl '%s': use 30m, 2h, 1d or plain seconds", text)
	}
	n, err := strconv.ParseInt(m[1], 10, 64)
	if err != nil {
		die("invalid --ttl '%s': use 30m, 2h, 1d or plain seconds", text)
	}
	switch m[2] {
	case "m":
		return n * 60
	case "h":
		return n * 3600
	case "d":
		return n * 86400
	}
	return n
}

const asciiSpace = " \t\n\r\f\v"

// sanitize flattens a note onto one line with no leading or trailing space.
func sanitize(s string) string {
	s = strings.NewReplacer("\n", " ", "\r", " ", "\t", " ").Replace(s)
	return strings.Trim(s, asciiSpace)
}

func jsonString(s string) string {
	s = sanitize(s)
	s = strings.ReplaceAll(s, `\`, `\\`)
	s = strings.ReplaceAll(s, `"`, `\"`)
	return `"` + s + `"`
}

// releaseLock drops the claim guard. rmdir in the shell version fails when the
// directory holds anything; os.Remove fails the same way.
func releaseLock() {
	if lockHeld {
		_ = os.Remove(lockDir)
		lockHeld = false
	}
}

// acquireLock serializes claim: checking for overlaps and then writing the
// lease is not one step, so two threads claiming the same paths at the same
// instant could both pass the check. mkdir is atomic everywhere this runs, so
// the directory itself is the guard; it is held for the few milliseconds a
// claim takes.
func acquireLock() {
	_ = os.MkdirAll(leaseDir, 0o755)
	for attempts := 0; ; {
		if err := os.Mkdir(lockDir, 0o755); err == nil {
			lockHeld = true
			return
		}
		// A claim killed between mkdir and rmdir would block every later
		// claim, so drop the guard once it is clearly abandoned.
		if fi, err := os.Stat(lockDir); err == nil {
			if time.Since(fi.ModTime()) > staleAfter {
				_ = os.Remove(lockDir)
				continue
			}
		}
		attempts++
		if attempts >= claimTimeout {
			die("another claim has held %s for 10s; remove it if nothing is claiming", lockDir)
		}
		time.Sleep(claimInterval)
	}
}

// loadLease reads one lease file. A file that disappears or cannot be read
// reads as an empty lease, and callers skip it, which is what the shell version
// does when read fails.
func loadLease(path string) *lease {
	l := &lease{}
	data, err := os.ReadFile(path)
	if err != nil {
		return l
	}
	for _, line := range strings.Split(string(data), "\n") {
		key, value := line, line
		if i := strings.Index(line, "="); i >= 0 {
			key, value = line[:i], line[i+1:]
		}
		switch key {
		case "token":
			l.token = value
		case "thread":
			l.thread = value
		case "kind":
			l.kind = value
		case "note":
			l.note = value
		case "created_at":
			l.createdRaw = value
		case "expires_at":
			l.expiresRaw = value
		case "path":
			l.paths = append(l.paths, value)
		}
	}
	return l
}

// leaseFiles lists the lease files in name order, the order the shell glob
// produces.
func leaseFiles() []string {
	entries, err := os.ReadDir(leaseDir)
	if err != nil {
		return nil
	}
	files := make([]string, 0, len(entries))
	for _, e := range entries {
		if e.IsDir() || !strings.HasSuffix(e.Name(), ".lease") {
			continue
		}
		files = append(files, leaseDir+"/"+e.Name())
	}
	return files
}

func isMine(l *lease) bool {
	if ownToken != "" && l.token == ownToken {
		return true
	}
	if threadID != "" && l.thread == threadID {
		return true
	}
	return false
}

func describeLoaded(w io.Writer, l *lease) {
	age := humanize(nowUnix - l.createdAt())
	remaining := humanize(l.expiresAt() - nowUnix)
	fmt.Fprintf(w, "holder   %s\n", l.thread)
	fmt.Fprintf(w, "token    %s\n", l.token)
	fmt.Fprintf(w, "note     %s\n", l.note)
	fmt.Fprintf(w, "held     %s ago, expires in %s\n", age, remaining)
}

func pruneExpired() {
	for _, file := range leaseFiles() {
		l := loadLease(file)
		if l.expiresAt() <= nowUnix {
			_ = os.Remove(file)
		}
	}
}

func newToken() string {
	for {
		token := fmt.Sprintf("l-%d-%d-%d", nowUnix, os.Getpid(), rand.IntN(32768))
		if _, err := os.Stat(leaseDir + "/" + token + ".lease"); os.IsNotExist(err) {
			return token
		}
	}
}

// owner is ${thread:-$token}: a lease with no thread is owned by its token.
func owner(token string) string {
	if threadID != "" {
		return threadID
	}
	return token
}

// writeLease writes the exact key order and layout the shell version writes, so
// either implementation can read the other's files.
func writeLease(token, kind, note string, expires int64, paths []string) {
	file := leaseDir + "/" + token + ".lease"
	tmp := fmt.Sprintf("%s.tmp.%d", file, os.Getpid())

	var b strings.Builder
	fmt.Fprintf(&b, "token=%s\n", token)
	fmt.Fprintf(&b, "thread=%s\n", owner(token))
	fmt.Fprintf(&b, "kind=%s\n", kind)
	fmt.Fprintf(&b, "note=%s\n", note)
	fmt.Fprintf(&b, "created_at=%d\n", nowUnix)
	fmt.Fprintf(&b, "expires_at=%d\n", expires)
	for _, p := range paths {
		fmt.Fprintf(&b, "path=%s\n", p)
	}

	if err := os.WriteFile(tmp, []byte(b.String()), 0o644); err != nil {
		die("cannot write %s: %v", tmp, err)
	}
	if err := os.Rename(tmp, file); err != nil {
		die("cannot write %s: %v", file, err)
	}
}

func printJSONResult(token, note string, expires int64, paths []string) {
	var b strings.Builder
	fmt.Fprintf(&b, `{"ok":true,"token":%s,"thread":%s,"note":%s,"expires_at":%d,"paths":[`,
		jsonString(token), jsonString(owner(token)), jsonString(note), expires)
	for i, p := range paths {
		if i > 0 {
			b.WriteString(",")
		}
		b.WriteString(jsonString(p))
	}
	b.WriteString("]}\n")
	fmt.Print(b.String())
}

func cmdClaim(args []string) {
	kind := kindWork
	note := ""
	ttl := defaultWorkTTL
	asJSON := false
	var inputs []string

	for i := 0; i < len(args); {
		switch a := args[i]; {
		case a == "--note":
			note = sanitize(optionValue(args, i+1))
			i += 2
		case a == "--ttl":
			ttl = parseTTL(optionValue(args, i+1))
			i += 2
		case a == "--json":
			asJSON = true
			i++
		case a == "--":
			i++
			for ; i < len(args); i++ {
				inputs = append(inputs, args[i])
			}
		case strings.HasPrefix(a, "-"):
			die("unknown option: %s", a)
		default:
			inputs = append(inputs, a)
			i++
		}
	}

	if len(inputs) == 0 {
		die("claim needs at least one path or glob")
	}
	prefixes := make([]string, 0, len(inputs))
	for _, in := range inputs {
		prefixes = append(prefixes, normalize(in))
	}

	acquireLock()
	pruneExpired()

	conflict := false
	seen := make(map[string]bool)
	for _, file := range leaseFiles() {
		l := loadLease(file)
		if l.kind != kind {
			continue
		}
		if isMine(l) {
			continue
		}
		for _, theirs := range l.paths {
			for _, mine := range prefixes {
				if !overlaps(mine, theirs) {
					continue
				}
				conflict = true
				if seen[l.token] {
					continue
				}
				seen[l.token] = true
				fmt.Fprintf(os.Stderr, "FAIL  %s lease refused: overlaps an active lease\n", kind)
				describeLoaded(os.Stderr, l)
				fmt.Fprintf(os.Stderr, "overlaps %s (theirs) and %s (yours)\n\n", theirs, mine)
				fmt.Fprint(os.Stderr, "Ask the holder to release it, or wait for it to expire:\n")
				fmt.Fprint(os.Stderr, "  script/lease status\n")
			}
		}
	}

	if conflict {
		exit(1)
	}

	expires := nowUnix + ttl
	token := newToken()
	writeLease(token, kind, note, expires, prefixes)
	releaseLock()

	if asJSON {
		printJSONResult(token, note, expires, prefixes)
		return
	}
	h := humanize(ttl)
	fmt.Printf("Claimed %s for %s as %s\n", strings.Join(prefixes, " "), h, owner(token))
	fmt.Printf("token=%s\n", token)
	fmt.Print("Export it so later commands and the commit hook recognize it:\n")
	fmt.Printf("  export AMP_LEASE_TOKEN=%s\n", token)
}

func cmdRelease(args []string) {
	target := ""
	if len(args) > 0 {
		target = args[0]
	}
	if target == "" {
		die("usage: script/lease release <token>|--all-mine")
	}

	if target == "--all-mine" {
		released := 0
		for _, file := range leaseFiles() {
			l := loadLease(file)
			if isMine(l) {
				_ = os.Remove(file)
				released++
			}
		}
		if released == 0 {
			fmt.Println("No leases of this thread were active.")
		} else {
			fmt.Printf("Released %d lease(s).\n", released)
		}
		return
	}

	file := leaseDir + "/" + target + ".lease"
	if _, err := os.Stat(file); err != nil {
		die("no lease with token %s (see script/lease status)", target)
	}
	_ = os.Remove(file)
	fmt.Printf("Released %s.\n", target)
}

func cmdHeartbeat(args []string) {
	token := ""
	if len(args) > 0 {
		token = args[0]
	}
	if token == "" {
		die("usage: script/lease heartbeat <token> [--ttl <time>]")
	}
	rest := args[1:]

	ttl := defaultWorkTTL
	for i := 0; i < len(rest); {
		switch a := rest[i]; {
		case a == "--ttl":
			ttl = parseTTL(optionValue(rest, i+1))
			i += 2
		default:
			die("unknown option: %s", a)
		}
	}

	file := leaseDir + "/" + token + ".lease"
	if _, err := os.Stat(file); err != nil {
		die("no lease with token %s (see script/lease status)", token)
	}

	expires := nowUnix + ttl
	data, err := os.ReadFile(file)
	if err != nil {
		die("cannot read %s: %v", file, err)
	}
	var b strings.Builder
	for _, line := range strings.Split(string(data), "\n") {
		if line == "" {
			continue
		}
		key, value := line, ""
		if i := strings.Index(line, "="); i >= 0 {
			key, value = line[:i], line[i+1:]
		}
		if key == "" {
			continue
		}
		if key == "expires_at" {
			fmt.Fprintf(&b, "expires_at=%d\n", expires)
			continue
		}
		fmt.Fprintf(&b, "%s=%s\n", key, value)
	}

	tmp := fmt.Sprintf("%s.tmp.%d", file, os.Getpid())
	if err := os.WriteFile(tmp, []byte(b.String()), 0o644); err != nil {
		die("cannot write %s: %v", tmp, err)
	}
	if err := os.Rename(tmp, file); err != nil {
		die("cannot write %s: %v", file, err)
	}

	h := humanize(ttl)
	fmt.Printf("Extended %s by %s (expires in %s).\n", token, h, h)
}

func cmdBuild(args []string) {
	action := ""
	if len(args) > 0 {
		action = args[0]
	}
	rest := args[1:]

	switch action {
	case "claim":
		note := ""
		ttl := defaultBuildTTL
		asJSON := false
		for i := 0; i < len(rest); {
			switch a := rest[i]; {
			case a == "--note":
				note = sanitize(optionValue(rest, i+1))
				i += 2
			case a == "--ttl":
				ttl = parseTTL(optionValue(rest, i+1))
				i += 2
			case a == "--json":
				asJSON = true
				i++
			default:
				die("unknown option: %s", a)
			}
		}

		acquireLock()
		pruneExpired()

		for _, file := range leaseFiles() {
			l := loadLease(file)
			if l.kind != kindBuild {
				continue
			}
			fmt.Fprint(os.Stderr, "FAIL  the build lock is already held\n")
			describeLoaded(os.Stderr, l)
			fmt.Fprint(os.Stderr, "\nA release-fast build and packaging share target dirs, so\n")
			fmt.Fprint(os.Stderr, "only one may run at a time. See script/lease status.\n")
			exit(1)
		}

		expires := nowUnix + ttl
		token := newToken()
		writeLease(token, kindBuild, note, expires, nil)
		releaseLock()

		if asJSON {
			printJSONResult(token, note, expires, nil)
			return
		}
		h := humanize(ttl)
		fmt.Printf("Holding the build lock for %s as %s\n", h, owner(token))
		fmt.Printf("token=%s\n", token)
		if note != "" {
			fmt.Printf("note=%s\n", note)
		}
	case "release":
		released := false
		for _, file := range leaseFiles() {
			l := loadLease(file)
			if l.kind != kindBuild {
				continue
			}
			_ = os.Remove(file)
			released = true
		}
		if released {
			fmt.Println("Build lock released.")
		} else {
			fmt.Println("No build lock was held.")
		}
	default:
		die("usage: script/lease build claim|release")
	}
}

func cmdStatus(args []string) {
	asJSON := len(args) > 0 && args[0] == "--json"

	_ = os.MkdirAll(leaseDir, 0o755)
	pruneExpired()

	files := leaseFiles()
	if len(files) == 0 {
		if asJSON {
			fmt.Println(`{"leases":[]}`)
		} else {
			fmt.Println("No active leases.")
		}
		return
	}

	if asJSON {
		var b strings.Builder
		b.WriteString(`{"leases":[`)
		for i, file := range files {
			if i > 0 {
				b.WriteString(",")
			}
			l := loadLease(file)
			fmt.Fprintf(&b, `{"token":%s,"thread":%s,"kind":%s,"note":%s,"created_at":%s,"expires_at":%s,"paths":[`,
				jsonString(l.token), jsonString(l.thread), jsonString(l.kind), jsonString(l.note),
				numOrZero(l.createdRaw), numOrZero(l.expiresRaw))
			for j, p := range l.paths {
				if j > 0 {
					b.WriteString(",")
				}
				b.WriteString(jsonString(p))
			}
			b.WriteString("]}")
		}
		b.WriteString("]}\n")
		fmt.Print(b.String())
		return
	}

	fmt.Println("ACTIVE LEASES")
	for _, file := range files {
		l := loadLease(file)
		label := l.kind
		switch l.kind {
		case kindWork:
			label = "WORK"
		case kindBuild:
			label = "BUILD"
		}
		fmt.Printf("\n%s\n", label)
		describeLoaded(os.Stdout, l)
		for _, p := range l.paths {
			if p != "" {
				fmt.Printf("path     %s\n", p)
			}
		}
	}
}

func cmdCheck(args []string) {
	mode := ""
	rangeSpec := ""
	extraToken := ""
	build := false
	var explicit []string

	for i := 0; i < len(args); {
		switch a := args[i]; {
		case a == "--staged":
			mode = "staged"
			i++
		case a == "--range":
			mode = "range"
			rangeSpec = optionValue(args, i+1)
			i += 2
		case a == "--paths":
			mode = "paths"
			i++
			for ; i < len(args) && !strings.HasPrefix(args[i], "--"); i++ {
				explicit = append(explicit, args[i])
			}
		case a == "--token":
			extraToken = optionValue(args, i+1)
			i += 2
		case a == "--build":
			build = true
			i++
		default:
			die("unknown option: %s", a)
		}
	}

	if fi, err := os.Stat(leaseDir); err != nil || !fi.IsDir() {
		return
	}
	pruneExpired()

	if build {
		for _, file := range leaseFiles() {
			l := loadLease(file)
			if l.kind != kindBuild {
				continue
			}
			describeLoaded(os.Stderr, l)
			exit(1)
		}
		return
	}

	if mode == "" {
		die("usage: script/lease check --staged|--paths ...|--range ...|--build")
	}

	var files []string
	switch mode {
	case "staged":
		files = gitNameOnly("diff", "--cached", "--name-only", "--diff-filter=ACMR")
	case "range":
		files = gitNameOnly("diff", "--name-only", rangeSpec)
	case "paths":
		files = explicit
	}
	if len(files) == 0 {
		return
	}

	mine := make([]string, 0, len(files))
	for _, f := range files {
		mine = append(mine, normalize(f))
	}

	conflict := false
	seen := make(map[string]bool)
	for _, file := range leaseFiles() {
		l := loadLease(file)
		if l.kind != kindWork {
			continue
		}
		if extraToken != "" && l.token == extraToken {
			continue
		}
		if isMine(l) {
			continue
		}
		for _, theirs := range l.paths {
			for _, prefix := range mine {
				if !overlaps(prefix, theirs) {
					continue
				}
				conflict = true
				if seen[l.token] {
					continue
				}
				seen[l.token] = true
				describeLoaded(os.Stderr, l)
				fmt.Fprintf(os.Stderr, "overlaps %s (theirs) and %s (yours)\n\n", theirs, prefix)
			}
		}
	}

	if !conflict {
		return
	}
	exit(1)
}

// gitNameOnly collects the file names a git command prints, one per line. A
// failing git command yields no names, which is what the shell version does
// when its process substitution fails.
func gitNameOnly(args ...string) []string {
	cmd := exec.Command("git", args...)
	cmd.Stderr = io.Discard
	out, err := cmd.Output()
	if err != nil && len(out) == 0 {
		return nil
	}
	var files []string
	for _, line := range strings.Split(string(out), "\n") {
		line = strings.TrimSuffix(line, "\r")
		if line != "" {
			files = append(files, line)
		}
	}
	return files
}

func optionValue(args []string, i int) string {
	if i < len(args) {
		return args[i]
	}
	return ""
}
