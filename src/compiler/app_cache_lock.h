#pragma once

#include <cerrno>
#include <fcntl.h>
#include <sys/file.h>
#include <unistd.h>

namespace naspeed {

enum class FileLockResult { Locked, Missing, Failed };

class SharedFileLock {
  int fd = -1;

public:
  SharedFileLock() = default;
  SharedFileLock(const SharedFileLock &) = delete;
  SharedFileLock &operator=(const SharedFileLock &) = delete;

  ~SharedFileLock() { unlock(); }

  FileLockResult lock(const char *path) {
    int opened;
    do {
      opened = open(path, O_RDONLY | O_CLOEXEC);
    } while (opened == -1 && errno == EINTR);
    if (opened == -1)
      return errno == ENOENT ? FileLockResult::Missing : FileLockResult::Failed;

    int result;
    do {
      result = flock(opened, LOCK_SH);
    } while (result == -1 && errno == EINTR);
    if (result == -1) {
      int error = errno;
      close(opened);
      errno = error;
      return FileLockResult::Failed;
    }
    fd = opened;
    return FileLockResult::Locked;
  }

  void unlock() {
    if (fd == -1)
      return;
    int result;
    do {
      result = flock(fd, LOCK_UN);
    } while (result == -1 && errno == EINTR);
    close(fd);
    fd = -1;
  }
};

} // namespace naspeed
