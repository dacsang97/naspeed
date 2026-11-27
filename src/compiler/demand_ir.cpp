#include <crt_externs.h>
#include <cstdlib>
#include <mutex>
#include <unistd.h>
#include <cerrno>
#include <fcntl.h>
#include <spawn.h>
#include <sys/socket.h>
#include <sys/wait.h>

#include "llvm/ADT/DenseMap.h"
#include "llvm/ADT/SmallString.h"
#include "llvm/ADT/StringExtras.h"
#include "llvm/Bitcode/BitcodeReader.h"
#include "llvm/ExecutionEngine/Orc/IRCompileLayer.h"
#include "llvm/ExecutionEngine/Orc/IndirectionUtils.h"
#include "llvm/ExecutionEngine/Orc/LLJIT.h"
#include "llvm/ExecutionEngine/Orc/LazyReexports.h"
#include "llvm/Support/Endian.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/Path.h"
#include "llvm/Support/SHA256.h"

namespace naspeed {
void storeObject(llvm::StringRef path, llvm::MemoryBufferRef object,
                 llvm::StringRef prefix);
bool loadDependencyImage(llvm::orc::JITDylib &jd, llvm::StringRef path);
}

namespace {

using namespace llvm;
using namespace llvm::orc;

class ObjectPayload final : public MemoryBuffer {
  std::unique_ptr<MemoryBuffer> record;

public:
  explicit ObjectPayload(std::unique_ptr<MemoryBuffer> record)
      : record(std::move(record)) {
    init(this->record->getBufferStart() + 32, this->record->getBufferEnd(), false);
  }
  StringRef getBufferIdentifier() const override {
    return record->getBufferIdentifier();
  }
  BufferKind getBufferKind() const override { return record->getBufferKind(); }
};

// A private socket inherited from the checked rustc process. Capped frames
// keep malformed or truncated producer responses from becoming large reads.
class DemandIR {
  int fd;
  pid_t worker = -1;
  std::string driver, recipe;
  SmallString<256> cache;
  SmallString<256> objects;
  std::string objectDomain;
  bool report;
  SmallVector<std::string> replay;
  static constexpr uint32_t maxName = 1024 * 1024;
  static constexpr uint32_t maxIR = 128 * 1024 * 1024;
  std::mutex mutex;
  LLLazyJIT &jit;
  DenseMap<SymbolStringPtr, bool> known;

  static Error fail(StringRef what) {
    return createStringError(inconvertibleErrorCode(), what);
  }

  bool transfer(bool send, char *data, size_t size) {
    while (size) {
      ssize_t count = send ? ::write(fd, data, size) : ::read(fd, data, size);
      if (count < 0 && errno == EINTR)
        continue;
      if (count <= 0)
        return false;
      data += count;
      size -= count;
    }
    return true;
  }

  bool receiveCount(uint32_t &len) {
    unsigned char bytes[4];
    if (!transfer(false, reinterpret_cast<char *>(bytes), sizeof(bytes)))
      return false;
    len = uint32_t(bytes[0]) | (uint32_t(bytes[1]) << 8) |
          (uint32_t(bytes[2]) << 16) | (uint32_t(bytes[3]) << 24);
    return true;
  }

  bool sendName(StringRef name, bool visit = false) {
    auto len = name.size() + visit;
    if (!len || len > maxName)
      return false;
    SmallString<256> request;
    for (int shift = 0; shift < 32; shift += 8)
      request.push_back(char((len >> shift) & 255));
    if (visit)
      request.push_back('!');
    request.append(name);
    return transfer(true, request.data(), request.size());
  }

  bool flushReplay() {
    for (const auto &previous : replay)
      if (!sendName(previous, true))
        return false;
    replay.clear();
    return true;
  }

  Error startWorker() {
    if (fd >= 0)
      return Error::success();
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) < 0)
      return errorCodeToError(std::error_code(errno, std::generic_category()));
    auto closePair = [&] {
      close(pair[0]);
      close(pair[1]);
    };
    int status = 0;
    int yes = 1;
    if (fcntl(pair[0], F_SETFD, FD_CLOEXEC) < 0 ||
        fcntl(pair[1], F_SETFD, FD_CLOEXEC) < 0 ||
        setsockopt(pair[0], SOL_SOCKET, SO_NOSIGPIPE, &yes, sizeof(yes)) < 0) {
      status = errno;
      closePair();
      return errorCodeToError(std::error_code(status, std::generic_category()));
    }
    posix_spawn_file_actions_t actions;
    if ((status = posix_spawn_file_actions_init(&actions)) != 0) {
      closePair();
      return errorCodeToError(std::error_code(status, std::generic_category()));
    }
    char option[] = "--naspeed-demand-recipe";
    char *args[] = {driver.data(), option, recipe.data(), nullptr};
    status = posix_spawn_file_actions_adddup2(&actions, pair[1], 3);
    if (status == 0)
      status = posix_spawn(&worker, driver.c_str(), &actions, nullptr, args,
                           *_NSGetEnviron());
    posix_spawn_file_actions_destroy(&actions);
    if (status != 0) {
      closePair();
      return errorCodeToError(std::error_code(status, std::generic_category()));
    }
    close(pair[1]);
    fd = pair[0];
    return Error::success();
  }

  static std::array<uint8_t, 32> digest(StringRef name, StringRef payload) {
    SHA256 hash;
    hash.update(name);
    hash.update(StringRef("\0", 1));
    hash.update(payload);
    return hash.final();
  }

  static StringRef bytes(const std::array<uint8_t, 32> &hash) {
    return StringRef(reinterpret_cast<const char *>(hash.data()), hash.size());
  }

  static bool takeCount(StringRef &packet, uint32_t &len) {
    if (packet.size() < 4)
      return false;
    len = support::endian::read32le(packet.data());
    packet = packet.drop_front(4);
    return true;
  }

public:
  DemandIR(LLLazyJIT &jit, StringRef driver, StringRef recipe,
           StringRef targetKey, bool report)
      : fd(recipe.empty() ? 3 : -1), driver(driver), recipe(recipe),
        report(report), jit(jit) {
    if (!recipe.empty()) {
      cache = recipe;
      sys::path::remove_filename(cache);
      auto generation = sys::path::filename(cache).str();
      sys::path::remove_filename(cache);
      objects = cache;
      sys::path::append(objects, "demand-objects-v1");
      if (sys::fs::create_directory(objects))
        objects.clear();
      else
        objectDomain = (Twine("naspeed-demand-object-v1 O0 ") + targetKey).str();
      sys::path::append(cache, "ir", generation);
    }
  }

  void finish() {
    {
      std::lock_guard lock(mutex);
      if (worker < 0)
        return;
      // Preserve validated products for cached callers reached after the
      // last miss, not just callers encountered before the next request.
      flushReplay();
      close(fd);
    }
    while (waitpid(worker, nullptr, 0) < 0 && errno == EINTR) {}
  }

  std::optional<bool> callable(const SymbolStringPtr &name) {
    std::lock_guard lock(mutex);
    auto it = known.find(name);
    if (it != known.end())
      return it->second;
    return std::nullopt;
  }

  Error add(JITDylib &jd, StringRef name) {
    std::array<uint8_t, 32> irDigest;
    std::unique_ptr<MemoryBuffer> hit;
    std::string received;
    StringRef packet;
    {
      std::lock_guard lock(mutex);
      SmallString<256> file;
      if (!cache.empty()) {
        file = cache;
        sys::path::append(file, toHex(SHA256::hash(arrayRefFromStringRef(name)), true));
        if (auto stored = MemoryBuffer::getFile(file, false, false)) {
          auto bytes = (*stored)->getBuffer();
          if (bytes.size() > 32 && bytes.size() - 32 <= maxIR) {
            irDigest = digest(name, bytes.drop_front(32));
            if (bytes.take_front(32) == DemandIR::bytes(irDigest)) {
              hit = std::move(*stored);
              packet = bytes.drop_front(32);
              replay.emplace_back(name);
              if (report)
                errs() << "naspeed demand IR cache hit: " << name << '\n';
            }
          }
        }
      }
      if (!hit) {
        if (auto error = startWorker())
          return error;
        // Cached callers can expose a new branch. Restore their dependency
        // discoveries in the compiler without regenerating their LLVM IR.
        if (!flushReplay())
          return fail("cannot replay cached symbols in rustc");
        uint32_t len;
        if (!sendName(name) || !receiveCount(len) || !len || len > maxIR)
          return fail("cannot read function record from rustc");
        received.resize(len);
        if (!transfer(false, received.data(), len))
          return fail("truncated function record from rustc");
        packet = received;
        if (!objects.empty())
          irDigest = digest(name, packet);
      }
      uint32_t count;
      if (!takeCount(packet, count) || count > maxName)
        return fail("invalid dependency table in function record");
      for (uint32_t i = 0; i < count; ++i) {
        if (packet.empty())
          return fail("truncated dependency table in function record");
        unsigned char tag = packet.front();
        packet = packet.drop_front();
        uint32_t len;
        if (tag > 1 || !takeCount(packet, len) || !len || len > packet.size())
          return fail("invalid dependency in function record");
        known.try_emplace(jit.mangleAndIntern(packet.take_front(len)), tag == 1);
        packet = packet.drop_front(len);
      }
    }
    SmallString<256> path;
    std::string key;
    if (!objects.empty()) {
      // The verified record already binds the complete IR and its symbol
      // table. Do not parse and re-serialize it just to key a machine object.
      key = toHex(digest(objectDomain, bytes(irDigest)), true);
      path = objects;
      sys::path::append(path, key);
      if (auto stored = MemoryBuffer::getFile(path, false, false)) {
        auto data = (*stored)->getBuffer();
        if (data.size() > 32 && data.size() - 32 <= maxIR &&
            data.take_front(32) == bytes(digest(key, data.drop_front(32)))) {
          if (report)
            errs() << "naspeed demand object hit: " << name << '\n';
          return jit.addObjectFile(jd, std::make_unique<ObjectPayload>(std::move(*stored)));
        }
      }
    }
    auto context = std::make_unique<LLVMContext>();
    auto mod = parseBitcodeFile(MemoryBufferRef(packet, name), *context);
    if (!mod)
      return mod.takeError();
    if (!(*mod)->getModuleInlineAsm().empty() ||
        llvm::any_of((*mod)->globals(), [](const GlobalVariable &g) {
          return g.hasSection() && g.getSection().contains("__mod_init_func");
        }))
      return fail("native assembly and Mach-O constructors are unsupported");
    if (path.empty())
      return jit.addIRModule(jd, ThreadSafeModule(std::move(*mod), std::move(context)));
    auto object = jit.getIRCompileLayer().getCompiler()(**mod);
    if (!object)
      return object.takeError();
    auto signature = digest(key, (*object)->getBuffer());
    naspeed::storeObject(path, (*object)->getMemBufferRef(), bytes(signature));
    return jit.addObjectFile(jd, std::move(*object));
  }

  struct Bodies final : DefinitionGenerator {
    DemandIR &ir;
    explicit Bodies(DemandIR &ir) : ir(ir) {}
    Error tryToGenerate(LookupState &, LookupKind, JITDylib &jd,
                        JITDylibLookupFlags,
                        const SymbolLookupSet &symbols) override {
      for (const auto &[symbol, flags] : symbols) {
        if (!ir.callable(symbol).has_value())
          continue;
        StringRef name = *symbol;
        // This runner is Darwin-only. ORC names have the Mach-O prefix.
        if (!name.consume_front("_"))
          return fail("rustc demand symbol has no Mach-O prefix");
        if (auto e = ir.add(jd, name))
          return e;
      }
      return Error::success();
    }
  };

  struct Stubs final : DefinitionGenerator {
    DemandIR &ir;
    JITDylib &bodies;
    LazyCallThroughManager &calls;
    RedirectableSymbolManager &stubs;
    Stubs(DemandIR &ir, JITDylib &bodies, LazyCallThroughManager &calls,
          RedirectableSymbolManager &stubs)
        : ir(ir), bodies(bodies), calls(calls), stubs(stubs) {}
    Error tryToGenerate(LookupState &, LookupKind, JITDylib &jd,
                        JITDylibLookupFlags,
                        const SymbolLookupSet &symbols) override {
      SymbolAliasMap functions, data;
      for (const auto &[symbol, flags] : symbols) {
        auto callable = ir.callable(symbol);
        if (!callable.has_value())
          continue;
        if (*callable)
          functions.try_emplace(symbol, symbol,
                                JITSymbolFlags::Exported | JITSymbolFlags::Callable);
        else
          data.try_emplace(symbol, symbol, JITSymbolFlags::Exported);
      }
      if (!functions.empty())
        if (auto e = jd.define(lazyReexports(calls, stubs, bodies, std::move(functions))))
          return e;
      if (!data.empty())
        return jd.define(reexports(bodies, std::move(data)));
      return Error::success();
    }
  };
};

} // namespace

namespace naspeed {

[[noreturn]] void runDemandGuest(llvm::orc::LLLazyJIT &jit,
                                llvm::orc::ExecutorAddr failure,
                                llvm::StringRef driver, llvm::StringRef recipe,
                                llvm::StringRef stdPath,
                                llvm::ArrayRef<llvm::StringRef> archives,
                                llvm::StringRef image, llvm::StringRef targetKey,
                                bool report,
                                int argc, char **argv) {
  ExitOnError check("naspeed demand IR: ", 125);
  if (recipe.empty() && fcntl(3, F_SETFD, FD_CLOEXEC) == -1) {
    errs() << "naspeed demand IR: missing compiler connection\n";
    std::exit(125);
  }
  auto &guest = jit.getMainJITDylib();
  auto &std = check(jit.loadPlatformDynamicLibrary(stdPath.str().c_str()));
  guest.addToLinkOrder(std);
  check(jit.linkStaticLibraryInto(guest, NASPEED_CLANG_BUILTINS));
  bool mapped = !image.empty() && naspeed::loadDependencyImage(guest, image);
  if (report && !image.empty())
    errs() << (mapped ? "naspeed dependency image mapped: "
                      : "naspeed dependency image rejected; using archives: ")
           << image << '\n';
  if (!mapped)
    for (auto archive : archives)
      check(jit.linkStaticLibraryInto(guest, archive.str().c_str()));
  auto &bodies = check(jit.createJITDylib("naspeed.body"));
  // Relocations in a body must see stubs before trying body generation.
  // LazyCallThroughManager looks up the chosen body directly in `bodies`.
  auto order = jit.defaultLinkOrder();
  order.insert(order.begin(), {&bodies, JITDylibLookupFlags::MatchAllSymbols});
  order.insert(order.begin(), {&guest, JITDylibLookupFlags::MatchAllSymbols});
  bodies.setLinkOrder(std::move(order), false);
  bodies.addToLinkOrder(std);
  auto calls = check(createLocalLazyCallThroughManager(
      jit.getTargetTriple(), jit.getExecutionSession(), failure));
  auto stubs = createLocalIndirectStubsManagerBuilder(jit.getTargetTriple())();
  DemandIR ir(jit, driver, recipe, targetKey, report);
  static DemandIR *active = &ir;
  std::atexit([] { active->finish(); });
  bodies.addGenerator(std::make_unique<DemandIR::Bodies>(ir));
  guest.addGenerator(std::make_unique<DemandIR::Stubs>(ir, bodies, *calls, *stubs));
  check(ir.add(bodies, "main"));
  *_NSGetArgc() = argc;
  *_NSGetArgv() = argv;
  setprogname(argv[0]);
  check(jit.initialize(bodies));
  auto entry = check(jit.lookup(bodies, "main")).toPtr<int(int, char **)>();
  int status = entry(argc, argv);
  check(jit.deinitialize(bodies));
  std::exit(status);
}

} // namespace naspeed
