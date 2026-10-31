// naspeed test-input tracer.
//
// Loaded into a test binary by `ns test` (DYLD_INSERT_LIBRARIES on macOS,
// LD_PRELOAD on Linux), it appends one line per input the test reads to the
// file named by NASPEED_TRACE_LOG:
//
//   I                 the tracer is loaded (one per process image)
//   E <name>          getenv
//   R <path>          open/openat for reading
//   S <path>          stat/lstat/fstatat/statx/access/realpath
//   D <path>          opendir
//   X <path-or-name>  execve/execvp/posix_spawn(p)
//
// Paths are made absolute with the current dir (or the dir fd). Writes and
// creations are not inputs and are not logged. The tracer never changes what
// a call returns; any failure only loses log lines.

#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <limits.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

#define LOG_ENV "NASPEED_TRACE_LOG"

static int log_fd = -1;
// Set once the tracer's constructor has run. libSystem/glibc call some of
// these functions (getenv during malloc setup) before thread-local storage
// of an inserted library works; until then every wrapper only forwards.
static volatile int ready;

// No recursion guard is needed: calls made from inside this library reach
// the real functions (dyld does not interpose the inserting image; on Linux
// the originals are looked up with RTLD_NEXT), and no thread-local state is
// used, since TLS in an inserted library is not usable during libc setup.
static int is_busy(void) { return !ready; }
static void set_busy(int v) { (void)v; }

#if defined(__APPLE__)
#include <crt_externs.h>
#include <fcntl.h>
#include <spawn.h>
#include <sys/stat.h>

// getenv without calling getenv: inside an inserted library dyld routes our
// own getenv calls to the interposed one, which would recurse.
static char *env_lookup(const char *name) {
    char **env = *_NSGetEnviron();
    size_t n = strlen(name);
    for (; env && *env; env++) {
        if (strncmp(*env, name, n) == 0 && (*env)[n] == '=') return *env + n + 1;
    }
    return NULL;
}
#else
// Linux: no sys/stat.h or fcntl.h, so our definitions cannot clash with
// header wrappers of any glibc version; buffers are passed through opaque.
#define O_ACCMODE_ 3
#define O_RDONLY_ 0
#define O_CREAT_ 0100
#define O_TMPFILE_ 020000000
#define AT_FDCWD_ (-100)
#define F_SETFD_ 2
#define FD_CLOEXEC_ 1
int fcntl(int fd, int cmd, ...);
#endif

#if defined(__APPLE__)
#define O_ACCMODE_ O_ACCMODE
#define O_RDONLY_ O_RDONLY
#define O_CREAT_ O_CREAT
#define AT_FDCWD_ AT_FDCWD
#endif

static int raw_open(const char *path, int flags, int mode);

static void open_log(void) {
    if (log_fd >= 0) return;
    const char *p;
#if defined(__APPLE__)
    p = env_lookup(LOG_ENV);
#else
    static char *(*real_getenv)(const char *);
    if (!real_getenv) real_getenv = (char *(*)(const char *))dlsym(RTLD_NEXT, "getenv");
    p = real_getenv ? real_getenv(LOG_ENV) : NULL;
#endif
    if (!p || !*p) return;
    // O_WRONLY | O_APPEND | O_CREAT, owner read/write.
#if defined(__APPLE__)
    log_fd = raw_open(p, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0600);
#else
    log_fd = raw_open(p, 01 | 02000 | 0100 | 02000000, 0600);
#endif
}

static void write_line(char kind, const char *a, const char *b) {
    if (log_fd < 0) return;
    char line[PATH_MAX * 2 + 8];
    size_t n = 0;
    line[n++] = kind;
    line[n++] = '\t';
    size_t la = a ? strlen(a) : 0, lb = b ? strlen(b) : 0;
    if (la + lb + 4 >= sizeof line) return;
    if (la) { memcpy(line + n, a, la); n += la; }
    if (lb) { memcpy(line + n, b, lb); n += lb; }
    line[n++] = '\n';
    // One write per line: O_APPEND keeps concurrent processes' lines whole.
    ssize_t w = write(log_fd, line, n);
    (void)w;
}

// Directory of `dirfd`, or the cwd for AT_FDCWD; empty on failure.
static int dir_of(int dirfd, char *out, size_t len) {
    if (dirfd == AT_FDCWD_) return getcwd(out, len) ? 0 : -1;
#if defined(__APPLE__)
    char buf[PATH_MAX];
    if (fcntl(dirfd, F_GETPATH, buf) == -1) return -1;
    strncpy(out, buf, len - 1);
    out[len - 1] = 0;
    return 0;
#else
    char link[64];
    int k = 0;
    const char *pre = "/proc/self/fd/";
    while (pre[k]) { link[k] = pre[k]; k++; }
    char digits[16];
    int d = 0, v = dirfd;
    if (v < 0) return -1;
    do { digits[d++] = (char)('0' + v % 10); v /= 10; } while (v && d < 15);
    while (d) link[k++] = digits[--d];
    link[k] = 0;
    ssize_t r = readlink(link, out, len - 1);
    if (r <= 0) return -1;
    out[r] = 0;
    return 0;
#endif
}

static void log_path_at(char kind, int dirfd, const char *path) {
    if (!path || !*path || is_busy()) return;
    set_busy(1);
    open_log();
    if (path[0] == '/') {
        write_line(kind, path, NULL);
    } else {
        char dir[PATH_MAX];
        if (dir_of(dirfd, dir, sizeof dir) == 0) {
            size_t l = strlen(dir);
            if (l + 1 < sizeof dir) { dir[l] = '/'; dir[l + 1] = 0; }
            write_line(kind, dir, path);
        }
    }
    set_busy(0);
}

static void log_path(char kind, const char *path) { log_path_at(kind, AT_FDCWD_, path); }

static void log_exec(const char *file) {
    if (!file || is_busy()) return;
    if (strchr(file, '/')) { log_path('X', file); return; }
    set_busy(1);
    open_log();
    write_line('X', file, NULL); // bare name: resolved through PATH by naspeed
    set_busy(0);
}

static int reads_only(int flags) {
    return (flags & O_ACCMODE_) == O_RDONLY_ && !(flags & O_CREAT_);
}

// ---------------------------------------------------------------- macOS

#if defined(__APPLE__)

static int raw_open(const char *path, int flags, int mode) { return open(path, flags, mode); }

#define INTERPOSE(replacement, original)                                         \
    __attribute__((used)) static struct {                                        \
        const void *r;                                                           \
        const void *o;                                                           \
    } interpose_##original __attribute__((section("__DATA,__interpose"))) = {    \
        (const void *)(unsigned long)&replacement, (const void *)(unsigned long)&original}

static char *t_getenv(const char *name) {
    if (name && !is_busy()) {
        set_busy(1);
        open_log();
        write_line('E', name, NULL);
        set_busy(0);
    }
    return name ? env_lookup(name) : NULL;
}
INTERPOSE(t_getenv, getenv);

static int t_open(const char *path, int flags, ...) {
    int mode = 0;
    if (flags & O_CREAT) {
        va_list ap;
        va_start(ap, flags);
        mode = va_arg(ap, int);
        va_end(ap);
    }
    if (reads_only(flags)) log_path('R', path);
    return open(path, flags, mode);
}
INTERPOSE(t_open, open);

static int t_openat(int fd, const char *path, int flags, ...) {
    int mode = 0;
    if (flags & O_CREAT) {
        va_list ap;
        va_start(ap, flags);
        mode = va_arg(ap, int);
        va_end(ap);
    }
    if (reads_only(flags)) log_path_at('R', fd, path);
    return openat(fd, path, flags, mode);
}
INTERPOSE(t_openat, openat);

static int t_stat(const char *path, struct stat *buf) {
    log_path('S', path);
    return stat(path, buf);
}
INTERPOSE(t_stat, stat);

static int t_lstat(const char *path, struct stat *buf) {
    log_path('S', path);
    return lstat(path, buf);
}
INTERPOSE(t_lstat, lstat);

static int t_fstatat(int fd, const char *path, struct stat *buf, int flag) {
    log_path_at('S', fd, path);
    return fstatat(fd, path, buf, flag);
}
INTERPOSE(t_fstatat, fstatat);

static int t_access(const char *path, int mode) {
    log_path('S', path);
    return access(path, mode);
}
INTERPOSE(t_access, access);

static char *t_realpath(const char *path, char *resolved) {
    log_path('S', path);
    return realpath(path, resolved);
}
INTERPOSE(t_realpath, realpath);

static DIR *t_opendir(const char *path) {
    log_path('D', path);
    return opendir(path);
}
INTERPOSE(t_opendir, opendir);

// System binaries are arm64e on Apple silicon, and this library is not:
// with SIP on, dyld drops DYLD_INSERT_LIBRARIES for them anyway; with SIP
// off (CI runners), dyld would try to insert it and kill the process. So a
// system binary is started without the variable, exactly as SIP would.
static int is_system_path(const char *p) {
    static const char *const dirs[] = {"/bin/", "/sbin/", "/usr/bin/", "/usr/sbin/",
                                       "/usr/libexec/", "/System/", NULL};
    for (int i = 0; dirs[i]; i++)
        if (strncmp(p, dirs[i], strlen(dirs[i])) == 0) return 1;
    return 0;
}

// `file` as execvp would find it: itself when it has a slash, else the
// first executable match on PATH. Empty when nothing matches.
static void resolve_file(const char *file, char *out, size_t n) {
    out[0] = 0;
    if (!file || !*file) return;
    if (strchr(file, '/')) {
        snprintf(out, n, "%s", file);
        return;
    }
    const char *path = env_lookup("PATH");
    if (!path) path = "/usr/bin:/bin:/usr/sbin:/sbin";
    while (*path) {
        const char *end = strchr(path, ':');
        size_t len = end ? (size_t)(end - path) : strlen(path);
        char cand[PATH_MAX];
        if (len > 0 && len + strlen(file) + 2 <= sizeof cand) {
            memcpy(cand, path, len);
            cand[len] = '/';
            strcpy(cand + len + 1, file);
            if (access(cand, X_OK) == 0) {
                snprintf(out, n, "%s", cand);
                return;
            }
        }
        if (!end) break;
        path = end + 1;
    }
}

// A copy of `envp` (or of the process environment) without
// DYLD_INSERT_LIBRARIES; NULL when out of memory.
static char **without_insert(char *const envp[]) {
    char *const *src = envp ? envp : *_NSGetEnviron();
    size_t n = 0;
    while (src[n]) n++;
    char **dst = malloc((n + 1) * sizeof *dst);
    if (!dst) return NULL;
    size_t j = 0;
    for (size_t i = 0; i < n; i++)
        if (strncmp(src[i], "DYLD_INSERT_LIBRARIES=", 22) != 0) dst[j++] = src[i];
    dst[j] = NULL;
    return dst;
}

// The environment a child started from `file` gets: `envp` itself, or a
// stripped copy (*owned set) for a system binary.
static char *const *child_env(const char *file, char *const envp[], char ***owned) {
    char resolved[PATH_MAX];
    *owned = NULL;
    resolve_file(file, resolved, sizeof resolved);
    if (!resolved[0] || !is_system_path(resolved)) return envp;
    *owned = without_insert(envp);
    return *owned ? *owned : envp;
}

static int t_execve(const char *path, char *const argv[], char *const envp[]) {
    log_exec(path);
    char **owned;
    char *const *env = child_env(path, envp, &owned);
    int r = execve(path, argv, env);
    free(owned);
    return r;
}
INTERPOSE(t_execve, execve);

static int t_execvp(const char *file, char *const argv[]) {
    log_exec(file);
    char resolved[PATH_MAX];
    resolve_file(file, resolved, sizeof resolved);
    if (resolved[0] && is_system_path(resolved)) {
        char **env = without_insert(NULL);
        if (env) {
            int r = execve(resolved, argv, env);
            free(env);
            return r;
        }
    }
    return execvp(file, argv);
}
INTERPOSE(t_execvp, execvp);

static int t_posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *fa,
                         const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
    log_exec(path);
    char **owned;
    char *const *env = child_env(path, envp, &owned);
    int r = posix_spawn(pid, path, fa, attr, argv, env);
    free(owned);
    return r;
}
INTERPOSE(t_posix_spawn, posix_spawn);

static int t_posix_spawnp(pid_t *pid, const char *file, const posix_spawn_file_actions_t *fa,
                          const posix_spawnattr_t *attr, char *const argv[], char *const envp[]) {
    log_exec(file);
    char **owned;
    char *const *env = child_env(file, envp, &owned);
    int r = posix_spawnp(pid, file, fa, attr, argv, env);
    free(owned);
    return r;
}
INTERPOSE(t_posix_spawnp, posix_spawnp);

__attribute__((constructor)) static void tracer_init(void) {
    open_log();
    write_line('I', NULL, NULL);
    ready = 1;
}

// ---------------------------------------------------------------- Linux

#else

#define NEXT(type, name)                                   \
    static __typeof__((type)0) real_##name;                \
    if (!real_##name) real_##name = (type)dlsym(RTLD_NEXT, #name)

static int raw_open(const char *path, int flags, int mode) {
    NEXT(int (*)(const char *, int, ...), open);
    return real_open ? real_open(path, flags, mode) : -1;
}

char *getenv(const char *name) {
    NEXT(char *(*)(const char *), getenv);
    if (name && !is_busy()) {
        set_busy(1);
        open_log();
        write_line('E', name, NULL);
        set_busy(0);
    }
    return real_getenv ? real_getenv(name) : NULL;
}

char *secure_getenv(const char *name) {
    NEXT(char *(*)(const char *), secure_getenv);
    if (name && !is_busy()) {
        set_busy(1);
        open_log();
        write_line('E', name, NULL);
        set_busy(0);
    }
    return real_secure_getenv ? real_secure_getenv(name) : NULL;
}

#define OPEN_LIKE(name)                                                 \
    int name(const char *path, int flags, ...) {                        \
        NEXT(int (*)(const char *, int, ...), name);                    \
        int mode = 0;                                                   \
        if ((flags & O_CREAT_) || (flags & O_TMPFILE_) == O_TMPFILE_) { \
            va_list ap;                                                 \
            va_start(ap, flags);                                        \
            mode = va_arg(ap, int);                                     \
            va_end(ap);                                                 \
        }                                                               \
        if (reads_only(flags)) log_path('R', path);                     \
        return real_##name ? real_##name(path, flags, mode) : -1;       \
    }
OPEN_LIKE(open)
OPEN_LIKE(open64)

#define OPENAT_LIKE(name)                                                    \
    int name(int fd, const char *path, int flags, ...) {                     \
        NEXT(int (*)(int, const char *, int, ...), name);                    \
        int mode = 0;                                                        \
        if ((flags & O_CREAT_) || (flags & O_TMPFILE_) == O_TMPFILE_) {      \
            va_list ap;                                                      \
            va_start(ap, flags);                                             \
            mode = va_arg(ap, int);                                          \
            va_end(ap);                                                      \
        }                                                                    \
        if (reads_only(flags)) log_path_at('R', fd, path);                   \
        return real_##name ? real_##name(fd, path, flags, mode) : -1;        \
    }
OPENAT_LIKE(openat)
OPENAT_LIKE(openat64)

#define STAT_LIKE(name)                                         \
    int name(const char *path, void *buf) {                     \
        NEXT(int (*)(const char *, void *), name);              \
        log_path('S', path);                                    \
        return real_##name ? real_##name(path, buf) : -1;       \
    }
STAT_LIKE(stat)
STAT_LIKE(stat64)
STAT_LIKE(lstat)
STAT_LIKE(lstat64)

#define XSTAT_LIKE(name)                                          \
    int name(int ver, const char *path, void *buf) {              \
        NEXT(int (*)(int, const char *, void *), name);           \
        log_path('S', path);                                      \
        return real_##name ? real_##name(ver, path, buf) : -1;    \
    }
XSTAT_LIKE(__xstat)
XSTAT_LIKE(__xstat64)
XSTAT_LIKE(__lxstat)
XSTAT_LIKE(__lxstat64)

#define FSTATAT_LIKE(name)                                             \
    int name(int fd, const char *path, void *buf, int flag) {          \
        NEXT(int (*)(int, const char *, void *, int), name);           \
        log_path_at('S', fd, path);                                    \
        return real_##name ? real_##name(fd, path, buf, flag) : -1;    \
    }
FSTATAT_LIKE(fstatat)
FSTATAT_LIKE(fstatat64)

int statx(int fd, const char *path, int flags, unsigned int mask, void *buf) {
    NEXT(int (*)(int, const char *, int, unsigned int, void *), statx);
    log_path_at('S', fd, path);
    return real_statx ? real_statx(fd, path, flags, mask, buf) : -1;
}

int access(const char *path, int mode) {
    NEXT(int (*)(const char *, int), access);
    log_path('S', path);
    return real_access ? real_access(path, mode) : -1;
}

char *realpath(const char *path, char *resolved) {
    NEXT(char *(*)(const char *, char *), realpath);
    log_path('S', path);
    return real_realpath ? real_realpath(path, resolved) : NULL;
}

DIR *opendir(const char *path) {
    NEXT(DIR *(*)(const char *), opendir);
    log_path('D', path);
    return real_opendir ? real_opendir(path) : NULL;
}

int execve(const char *path, char *const argv[], char *const envp[]) {
    NEXT(int (*)(const char *, char *const[], char *const[]), execve);
    log_exec(path);
    return real_execve ? real_execve(path, argv, envp) : -1;
}

int execvp(const char *file, char *const argv[]) {
    NEXT(int (*)(const char *, char *const[]), execvp);
    log_exec(file);
    return real_execvp ? real_execvp(file, argv) : -1;
}

int posix_spawn(pid_t *pid, const char *path, const void *fa, const void *attr,
                char *const argv[], char *const envp[]) {
    NEXT(int (*)(pid_t *, const char *, const void *, const void *, char *const[], char *const[]),
         posix_spawn);
    log_exec(path);
    return real_posix_spawn ? real_posix_spawn(pid, path, fa, attr, argv, envp) : -1;
}

int posix_spawnp(pid_t *pid, const char *file, const void *fa, const void *attr,
                 char *const argv[], char *const envp[]) {
    NEXT(int (*)(pid_t *, const char *, const void *, const void *, char *const[], char *const[]),
         posix_spawnp);
    log_exec(file);
    return real_posix_spawnp ? real_posix_spawnp(pid, file, fa, attr, argv, envp) : -1;
}

__attribute__((constructor)) static void tracer_init(void) {
    open_log();
    if (log_fd >= 0) fcntl(log_fd, F_SETFD_, FD_CLOEXEC_);
    write_line('I', NULL, NULL);
    ready = 1;
}

#endif
