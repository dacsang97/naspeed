#include "app_cache_lock.h"
#include "llvm/ADT/DenseMap.h"
#include "llvm/ExecutionEngine/Orc/AbsoluteSymbols.h"
#include "llvm/ExecutionEngine/Orc/Core.h"
#include "llvm/Object/MachO.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/Path.h"
#include "llvm/Support/SHA256.h"

#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <crt_externs.h>
#include <cstdlib>
#include <dlfcn.h>
#include <mach-o/dyld.h>
#include <mach-o/nlist.h>
#include <fcntl.h>
#include <pthread.h>
#include <sys/event.h>
#include <unistd.h>

using namespace llvm;
using namespace llvm::orc;

namespace {

class ImageSymbols final : public DefinitionGenerator {
  std::unique_ptr<MemoryBuffer> buffer;
  DenseMap<StringRef, ExecutorSymbolDef> symbols;
  void *handle = nullptr;
  intptr_t slide = 0;

public:
  explicit ImageSymbols(std::unique_ptr<MemoryBuffer> buffer)
      : buffer(std::move(buffer)) {}
  ~ImageSymbols() override {
    if (handle)
      dlclose(handle);
  }

  bool open(StringRef path) {
    auto image = object::ObjectFile::createMachOObjectFile(buffer->getMemBufferRef());
    if (!image) {
      consumeError(image.takeError());
      return false;
    }
    if (!(*image)->is64Bit() || (*image)->getHeader64().filetype != MH_DYLIB ||
        (*image)->getHeader64().cputype != CPU_TYPE_ARM64)
      return false;
    SmallVector<bool> text{false}; // n_sect is one-based.
    for (const auto &section : (*image)->sections()) {
      auto raw = (*image)->getSection64(section.getRawDataRefImpl());
      auto type = raw.flags & SECTION_TYPE;
      // `dlopen` runs image initializers once at load and dyld allocates
      // native TLS normally, matching the archive path's startup semantics.
      // Reject images with mod-init pointers or ObjC metadata.
      if (type == S_MOD_INIT_FUNC_POINTERS ||
          type == S_MOD_TERM_FUNC_POINTERS ||
          type == S_THREAD_LOCAL_INIT_FUNCTION_POINTERS ||
          StringRef(raw.sectname, sizeof(raw.sectname)).starts_with("__objc")) {
        errs() << "naspeed dep image rejected section "
               << StringRef(raw.sectname, sizeof(raw.sectname)).str() << '\n';
        return false;
      }
      text.push_back(section.isText());
    }
    for (const auto &symbol : (*image)->symbols()) {
      auto raw = (*image)->getSymbol64TableEntry(symbol.getRawDataRefImpl());
      // ld64.lld retains private extern definitions in nlist but omits
      // them from dyld's export table. Rust can reference them from IR.
      if ((raw.n_type & N_STAB) ||
          (raw.n_type & N_TYPE) != N_SECT ||
          !(raw.n_type & (N_EXT | N_PEXT)))
        continue;
      if (raw.n_sect == 0 || raw.n_sect >= text.size())
        return false;
      auto name = symbol.getName();
      if (!name) {
        consumeError(name.takeError());
        return false;
      }
      JITSymbolFlags flags = JITSymbolFlags::Exported;
      if (text[raw.n_sect])
        flags |= JITSymbolFlags::Callable;
      if (raw.n_desc & N_WEAK_DEF)
        flags |= JITSymbolFlags::Weak;
      auto [it, inserted] = symbols.try_emplace(*name, ExecutorAddr(raw.n_value), flags);
      if (!inserted && it->second.getAddress() != ExecutorAddr(raw.n_value))
        return false;
    }
    handle = dlopen(path.str().c_str(), RTLD_NOW | RTLD_LOCAL);
    if (!handle)
      return false;
    // Snapshot count once; this process has not entered any guest code yet.
    for (uint32_t i = 0, n = _dyld_image_count(); i < n; ++i) {
      const char *name = _dyld_get_image_name(i);
      if (name && path == name) {
        slide = _dyld_get_image_vmaddr_slide(i);
        return true;
      }
    }
    return false;
  }

  Error tryToGenerate(LookupState &, LookupKind, JITDylib &jd,
                      JITDylibLookupFlags, const SymbolLookupSet &requested) override {
    SymbolMap found;
    for (const auto &[symbol, _] : requested) {
      auto it = symbols.find(*symbol);
      if (it != symbols.end()) {
        const auto &value = it->second;
        found.try_emplace(symbol, ExecutorAddr(value.getAddress().getValue() + slide),
                          value.getFlags());
      }
    }
    if (found.empty())
      return Error::success();
    return jd.define(absoluteSymbols(std::move(found)));
  }
};

} // namespace

namespace naspeed {

static std::unique_ptr<MemoryBuffer> verifiedImage(StringRef path, StringRef receipt) {
  auto digest = MemoryBuffer::getFile(receipt, false, false);
  if (!digest || (*digest)->getBufferSize() != 32)
    return {};
  auto image = MemoryBuffer::getFile(path, false, false);
  if (!image)
    return {};
  auto hash = SHA256::hash(arrayRefFromStringRef((*image)->getBuffer()));
  if ((*digest)->getBuffer() != StringRef(reinterpret_cast<const char *>(hash.data()), hash.size()))
    return {};
  return std::move(*image);
}

bool loadDependencyImage(JITDylib &jd, StringRef path) {
  SmallString<256> receipt(path);
  sys::path::remove_filename(receipt);
  sys::path::append(receipt, "sha256");
  auto image = verifiedImage(path, receipt);
  if (!image)
    return false;
  auto generator = std::make_unique<ImageSymbols>(std::move(image));
  if (!generator->open(path))
    return false;
  jd.addGenerator(std::move(generator));
  return true;
}

// Neither image may execute guest code until all slots are bound. Inspect
// both before dlopen: even an otherwise valid archive can carry an init hook.
static bool patchImage(StringRef path, StringRef base = {}) {
  SmallString<256> receipt(path);
  sys::path::remove_filename(receipt);
  sys::path::append(receipt, "app.sha256");
  auto bytes = verifiedImage(path, receipt);
  if (!bytes)
    return false;
  auto image = object::ObjectFile::createMachOObjectFile(bytes->getMemBufferRef());
  if (!image) {
    consumeError(image.takeError());
    return false;
  }
  if (!(*image)->is64Bit() || (*image)->getHeader64().filetype != MH_DYLIB ||
      (*image)->getHeader64().cputype != CPU_TYPE_ARM64)
    return false;
  bool matchesBase = base.empty();
  for (const auto &command : (*image)->load_commands()) {
    if (command.C.cmd == LC_ROUTINES || command.C.cmd == LC_ROUTINES_64)
      return false;
    if (!base.empty() && command.C.cmd == LC_LOAD_DYLIB) {
      auto offset = (*image)->getDylibIDLoadCommand(command).dylib.name;
      if (offset >= command.C.cmdsize)
        return false;
      auto name = StringRef(command.Ptr + offset, command.C.cmdsize - offset);
      auto end = name.find('\0');
      if (end == StringRef::npos)
        return false;
      matchesBase |= name.take_front(end) == base;
    }
  }
  for (const auto &section : (*image)->sections()) {
    auto raw = (*image)->getSection64(section.getRawDataRefImpl());
    auto type = raw.flags & SECTION_TYPE;
    // The base is permanent: `dlopen` runs its initializers once, matching
    // the archive path's startup semantics; `__init_offsets` stays legal
    // there but an unloadable patch must not own initializers.
    if (type == S_MOD_INIT_FUNC_POINTERS || type == S_MOD_TERM_FUNC_POINTERS ||
        (type == S_INIT_FUNC_OFFSETS && !base.empty()) ||
        StringRef(raw.sectname, sizeof(raw.sectname)).starts_with("__objc"))
      return false;
    // An unloaded function must not own TLS or a registered TLS callback.
    // References to TLS owned by the permanent base are still supported.
    if (!base.empty() &&
        (type == S_THREAD_LOCAL_REGULAR || type == S_THREAD_LOCAL_ZEROFILL ||
         type == S_THREAD_LOCAL_VARIABLES || type == S_THREAD_LOCAL_INIT_FUNCTION_POINTERS))
      return false;
  }
  return matchesBase;
}

namespace {

struct PatchPlan {
  StringRef base;
  SmallVector<StringRef, 8> patches;
  // Selector name pinned to each slot ("<index>:<leaf>"); empty for plans
  // written before slot names existed.
  SmallVector<StringRef, 8> names;
};

bool parsePatchPlan(StringRef bytes, PatchPlan &plan) {
  if (bytes.size() > 128 * 1024)
    return false;
  SmallVector<StringRef, 16> fields;
  bytes.split(fields, '\0', -1, true);
  if (fields.size() < 4 || fields.size() > 130 || fields.size() % 2 ||
      !fields.back().empty() || !sys::path::is_absolute(fields[0]))
    return false;
  plan.base = fields[0];
  for (size_t i = 1; i + 1 < fields.size(); i += 2) {
    auto [slot, name] = fields[i].split(':');
    if (slot != Twine((i - 1) / 2).str() ||
        !sys::path::is_absolute(fields[i + 1]))
      return false;
    plan.patches.push_back(fields[i + 1]);
    plan.names.push_back(slot.size() == fields[i].size() ? "" : name);
  }
  return true;
}

// ABI shared with the LLVM stubs. Both atomic counters live in writable
// data; no writes go to a code page.
struct Dispatch {
  uint64_t readers = 0;
  void **functions = nullptr;
};
static_assert(offsetof(Dispatch, functions) == 8 && sizeof(Dispatch) == 16);
static_assert(__atomic_always_lock_free(sizeof(uint64_t), nullptr));

struct Revision {
  Dispatch dispatch;
  SmallVector<std::shared_ptr<void>, 8> images;
  SmallVector<void *, 8> functions;
};

void *openImage(StringRef path) {
  return dlopen(path.str().c_str(), RTLD_NOW | RTLD_LOCAL | RTLD_FIRST);
}

struct Application {
  // Every base image that has ever run keeps its own table and gate
  // globals; old frames can only see the base they called into. A base is
  // never dlclosed: a blocked frame can hold it alive past any reclaim.
  SmallVector<std::shared_ptr<void>, 4> bases;
  SmallVector<Dispatch **, 4> tables;
  SmallVector<uint64_t *, 4> gates;
  SmallVector<std::unique_ptr<Revision>, 4> retired;
  std::unique_ptr<Revision> active;
  std::unique_ptr<MemoryBuffer> bytes;
  PatchPlan plan; // StringRefs borrow bytes.

  void registerBase(void *handle) {
    bases.emplace_back(handle, &dlclose);
    tables.push_back(
        static_cast<Dispatch **>(dlsym(handle, "__ns_patch_table_v2")));
    gates.push_back(
        static_cast<uint64_t *>(dlsym(handle, "__ns_patch_gate_v2")));
  }

  bool install(std::unique_ptr<MemoryBuffer> &candidate, PatchPlan &next) {
    // A rebuilt base means the guest's constants and globals moved; a
    // patch-only swap would call into a stale layout. Map the new base and
    // publish one dispatch through every table so old and new frames land
    // on the same revision.
    void *fresh = nullptr;
    if (active && next.base != plan.base) {
      if (!patchImage(next.base))
        return false;
      fresh = openImage(next.base);
      if (!fresh)
        return false;
    }
    auto revision = std::make_unique<Revision>();
    for (size_t i = 0; i < next.patches.size(); ++i) {
      auto path = next.patches[i];
      if (active && path == plan.patches[i]) {
        revision->images.push_back(active->images[i]);
        revision->functions.push_back(active->functions[i]);
        continue;
      }
      if (!patchImage(path, next.base)) {
        if (fresh)
          dlclose(fresh);
        return false;
      }
      auto *image = openImage(path);
      if (!image) {
        if (fresh)
          dlclose(fresh);
        return false;
      }
      auto *procedure = dlsym(image, (Twine("__ns_patch_impl_") + Twine(i)).str().c_str());
      if (!procedure) {
        dlclose(image);
        if (fresh)
          dlclose(fresh);
        return false;
      }
      revision->images.emplace_back(image, &dlclose);
      revision->functions.push_back(procedure);
    }
    if (fresh)
      registerBase(fresh);
    revision->dispatch.functions = revision->functions.data();
    Dispatch *published = &revision->dispatch;
    if (active)
      retired.push_back(std::move(active));
    active = std::move(revision);
    for (auto *table : tables)
      __atomic_store_n(table, published, __ATOMIC_SEQ_CST);
    plan = std::move(next);
    bytes = std::move(candidate);
    return true;
  }

  size_t reclaim() {
    // A caller that could have observed a retired dispatch must have
    // entered its base's gate ahead of the publishing store in the SC
    // order. Once every gate is empty, none can still be about to pin a
    // retired record. New callers can only acquire the current record.
    if (retired.empty())
      return 0;
    for (auto *gate : gates)
      if (__atomic_load_n(gate, __ATOMIC_SEQ_CST) != 0)
        return 0;
    size_t before = retired.size();
    retired.erase(std::remove_if(retired.begin(), retired.end(), [](const auto &revision) {
      return __atomic_load_n(&revision->dispatch.readers, __ATOMIC_ACQUIRE) == 0;
    }), retired.end());
    return before - retired.size();
  }
};

struct LiveWatch {
  Application &app;
  std::string feed;
  SmallString<256> scope;
  bool report;
  int directory = -1;
  int queue = -1;
  std::unique_ptr<MemoryBuffer> rejected;

  LiveWatch(Application &app, StringRef feed, bool report)
      : app(app), feed(feed), report(report) {
    scope = sys::path::parent_path(sys::path::parent_path(feed));
    sys::path::append(scope, "patch-gc.lock");
  }

  ~LiveWatch() {
    if (directory >= 0)
      close(directory);
    if (queue >= 0)
      close(queue);
  }

  bool arm() {
    auto path = sys::path::parent_path(feed);
    directory = open(path.str().c_str(), O_EVTONLY | O_CLOEXEC);
    if (directory < 0)
      return false;
    queue = kqueue();
    if (queue < 0 || fcntl(queue, F_SETFD, FD_CLOEXEC) == -1)
      return false;
    struct kevent change;
    EV_SET(&change, directory, EVFILT_VNODE, EV_ADD | EV_CLEAR,
           NOTE_WRITE | NOTE_RENAME | NOTE_DELETE | NOTE_REVOKE, 0, nullptr);
    return kevent(queue, &change, 1, nullptr, 0, nullptr) != -1;
  }

  void refresh() {
    SharedFileLock scopeLock;
    if (scopeLock.lock(scope.c_str()) == FileLockResult::Failed)
      return;
    using namespace std::chrono;
    auto start = steady_clock::now();
    auto file = MemoryBuffer::getFile(feed, false, false);
    if (!file)
      return;
    auto contents = (*file)->getBuffer();
    if (contents == app.bytes->getBuffer() ||
        (rejected && contents == rejected->getBuffer()))
      return;
    const char *reason = nullptr;
    PatchPlan plan;
    if (!parsePatchPlan(contents, plan))
      reason = "invalid or missing function plan";
    else if (app.plan.patches.size() != plan.patches.size() ||
             app.plan.names != plan.names)
      reason = "slot layout changed; restart required";
    else if (!app.install(*file, plan))
      reason = "function image missing, damaged or not loadable";
    if (reason) {
      rejected = std::move(*file);
      scopeLock.unlock();
      std::fprintf(stderr, "naspeed app live rejected: %s\n", reason);
      return;
    }
    scopeLock.unlock();
    rejected.reset();
    if (report) {
      auto elapsed = duration_cast<microseconds>(steady_clock::now() - start).count();
      std::fprintf(stderr, "naspeed app live applied: %lld us\n",
                   static_cast<long long>(elapsed));
    }
  }

  void collect() {
    size_t count = app.reclaim();
    if (report && count)
      std::fprintf(stderr, "naspeed app live reclaimed: %zu revisions\n", count);
  }

  static void *listen(void *opaque) {
    std::unique_ptr<LiveWatch> watch(static_cast<LiveWatch *>(opaque));
    if (watch->report)
      std::fprintf(stderr, "naspeed app live watching: %s\n", watch->feed.c_str());
    // The queue was armed before reading: an update that races startup is
    // either reflected in this first read or already waiting in the queue.
    watch->refresh();
    for (;;) {
      watch->collect();
      struct kevent event;
      // A blocked old frame can finish without another build. Only use a
      // timeout while there is retired code waiting on callers.
      const timespec retry{0, 20 * 1000 * 1000};
      int ready = kevent(watch->queue, nullptr, 0, &event, 1,
                         watch->app.retired.empty() ? nullptr : &retry);
      if (ready == 0)
        continue;
      if (ready == -1) {
        if (errno == EINTR)
          continue;
        break;
      }
      if ((event.flags & EV_ERROR) ||
          (event.fflags & (NOTE_RENAME | NOTE_DELETE | NOTE_REVOKE)))
        break;
      watch->refresh();
    }
    std::fputs("naspeed app live stopped watching; restart required\n", stderr);
    return nullptr;
  }
};

void startLiveWatch(Application &app, StringRef generation, bool report) {
  SmallString<256> path(generation);
  sys::path::append(path, "live-feed");
  auto file = MemoryBuffer::getFile(path, false, false);
  if (file) {
    auto feed = (*file)->getBuffer();
    if (sys::path::is_absolute(feed) && !feed.contains('\0')) {
      auto watch = std::make_unique<LiveWatch>(app, feed, report);
      if (watch->arm()) {
        // A detached worker owns its watch descriptors, never the app.
        auto *owned = watch.release();
        pthread_t thread;
        if (pthread_create(&thread, nullptr, &LiveWatch::listen, owned) == 0) {
          pthread_detach(thread);
          return;
        }
        delete owned;
      }
    }
  }
  std::fputs("naspeed app live unavailable: cannot watch build feed\n", stderr);
}

} // namespace

static bool launchPatched(std::unique_ptr<MemoryBuffer> bytes, StringRef generation,
                          bool report, int argc, char **argv) {
  PatchPlan plan;
  if (!parsePatchPlan(bytes->getBuffer(), plan) || !patchImage(plan.base))
    return false;
  auto *base = openImage(plan.base);
  if (!base)
    return false;
  auto entry = reinterpret_cast<int (*)(int, char **)>(dlsym(base, "main"));
  Application app;
  app.registerBase(base);
  if (!entry || !app.tables[0] || !app.gates[0] || !app.install(bytes, plan))
    return false;
  *_NSGetArgc() = argc;
  *_NSGetArgv() = argv;
  setprogname(argv[0]);
  if (report) {
    errs() << "naspeed app patch base mapped: " << app.plan.base << '\n';
    for (size_t i = 0; i < app.plan.patches.size(); ++i)
      errs() << "naspeed app patch mapped: " << i << ' ' << app.plan.patches[i] << '\n';
  }
  if (const char *enable = std::getenv("NASPEED_LAZY_APP_LIVE");
      enable && std::strcmp(enable, "1") == 0)
    startLiveWatch(app, generation, report);
  // std::exit does not destroy app. The current table and any still-pinned
  // retired tables remain valid through guest teardown.
  std::exit(entry(argc, argv));
}

bool runAppPatch(StringRef recipe, bool report, int argc, char **argv) {
  SmallString<256> plan(recipe);
  sys::path::remove_filename(plan);
  sys::path::append(plan, "patch-plan");
  auto file = MemoryBuffer::getFile(plan);
  if (!file)
    return false; // Ordinary app images have no patch plan.
  launchPatched(std::move(*file), sys::path::parent_path(plan), report, argc, argv);
  if (report)
    errs() << "naspeed app patch unavailable; using ORC\n";
  return true;
}

void runAppImage(StringRef recipe, bool report, int argc, char **argv) {
  SmallString<256> image(recipe);
  sys::path::remove_filename(image);
  SmallString<256> receipt(image);
  sys::path::append(image, "app.dylib");
  sys::path::append(receipt, "app.sha256");
  auto buffer = verifiedImage(image, receipt);
  if (!buffer) {
    if (report)
      errs() << "naspeed app image missing or invalid; using ORC\n";
    return;
  }
  buffer.reset();
  *_NSGetArgc() = argc;
  *_NSGetArgv() = argv;
  setprogname(argv[0]);
  auto *handle = dlopen(image.c_str(), RTLD_NOW | RTLD_LOCAL | RTLD_FIRST);
  if (!handle) {
    if (report)
      errs() << "naspeed app image rejected; using ORC: " << dlerror() << '\n';
    return;
  }
  auto entry = reinterpret_cast<int (*)(int, char **)>(dlsym(handle, "main"));
  if (!entry) {
    errs() << "naspeed app image has no C main\n";
    std::exit(125);
  }
  if (report)
    errs() << "naspeed app image mapped: " << image << '\n';
  // Keep the image mapped through guest atexit callbacks and TLS teardown.
  std::exit(entry(argc, argv));
}

} // namespace naspeed
