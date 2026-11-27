// The guest runs on the process's main thread. ORC owns stubs, relocations,
// executable pages and unwind registration for the lifetime of the process.
#include <crt_externs.h>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <unistd.h>
#include "app_cache_lock.h"

#include "llvm/ADT/StringExtras.h"
#include "llvm/Bitcode/BitcodeWriter.h"
#include "llvm/Config/llvm-config.h"
#include "llvm/ExecutionEngine/Orc/CompileUtils.h"
#include "llvm/ExecutionEngine/Orc/LLJIT.h"
#include "llvm/IRReader/IRReader.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/Path.h"
#include "llvm/Support/SHA256.h"
#include "llvm/Support/SourceMgr.h"
#include "llvm/Support/TargetSelect.h"

using namespace llvm;
using namespace llvm::orc;

namespace llvm { class Module; class TargetMachine; }
namespace naspeed {
llvm::Error emit(llvm::Module &module, llvm::TargetMachine &target,
                 llvm::StringRef output);
[[noreturn]] void runDemandGuest(LLLazyJIT &jit, ExecutorAddr failure,
                                StringRef driver, StringRef recipe,
                                StringRef stdPath, ArrayRef<StringRef> archives,
                                StringRef image, StringRef targetKey, bool report,
                                int argc, char **argv);
void runAppImage(StringRef recipe, bool report, int argc, char **argv);
bool runAppPatch(StringRef recipe, bool report, int argc, char **argv);
int prepareAppPatch(int argc, char **argv);

void storeObject(StringRef path, MemoryBufferRef object, StringRef prefix) {
  auto tmp = sys::fs::TempFile::create(Twine(path) + ".%%%%%%");
  if (!tmp) {
    consumeError(tmp.takeError());
    return;
  }
  raw_fd_ostream out(tmp->FD, /*shouldClose=*/false);
  out << prefix << object.getBuffer();
  out.flush();
  if (out.has_error()) {
    out.clear_error();
    consumeError(tmp->discard());
  } else {
    consumeError(tmp->keep(path));
  }
}
}

namespace {

class DigestStream final : public raw_ostream {
  SHA256 hash;
  uint64_t length = 0;

  void write_impl(const char *data, size_t size) override {
    hash.update(StringRef(data, size));
    length += size;
  }

  uint64_t current_pos() const override { return length; }

public:
  DigestStream() : raw_ostream(/*unbuffered=*/true) {}
  std::array<uint8_t, 32> finish() { return hash.final(); }
};

class CachedCompiler final : public TMOwningSimpleCompiler {
  SmallString<256> directory;
  std::string targetKey;
  bool report;
  std::mutex compileMutex;

public:
  CachedCompiler(std::unique_ptr<TargetMachine> tm, StringRef dir,
                 std::string key, bool report)
      : TMOwningSimpleCompiler(std::move(tm)), directory(dir),
        targetKey(std::move(key)), report(report) {}

  Expected<CompileResult> operator()(Module &module) override {
    SmallString<256> path;
    if (!directory.empty()) {
      DigestStream digest;
      digest << targetKey << '\0';
      // Use the actual post-partition IR, not a symbol name or source mtime.
      // Identical names can refer to different bodies or relocation targets.
      WriteBitcodeToFile(module, digest);
      path = directory;
      sys::path::append(path, toHex(digest.finish(), true));
      if (auto hit = MemoryBuffer::getFile(path, false, false)) {
        if (report)
          for (const auto &f : module)
            if (!f.isDeclaration())
              errs() << "naspeed lazy cache hit: " << f.getName() << '\n';
        return std::move(*hit);
      }
    }
    auto compile = [&] {
      // Guest threads can demand separate functions concurrently.
      std::lock_guard lock(compileMutex);
      return TMOwningSimpleCompiler::operator()(module);
    };
    auto object = compile();
    if (object && !path.empty())
      naspeed::storeObject(path, (*object)->getMemBufferRef(), {});
    return object;
  }
};

[[noreturn]] static void materializationFailed() {
  // The failed function can have any signature; never return through its stub.
  // Avoid destroying the JIT from inside a failing JIT frame.
  _exit(125);
}

static int run(int argc, char **argv) {
  ExitOnError check("naspeed lazy run: ", 125);
  bool demand = std::strcmp(argv[1], "--demand-ir") == 0;
  if (demand && argc < 4)
    return 125;

  const char *trace = std::getenv("NASPEED_LAZY_RUN_REPORT");
  bool report = trace && std::strcmp(trace, "1") == 0;
  std::unique_ptr<MemoryBuffer> manifest;
  SmallVector<StringRef> fields;
  naspeed::SharedFileLock generationLease;
  if (!demand) {
    SmallString<256> generation(argv[1]);
    sys::path::remove_filename(generation);
    SmallString<256> scope(generation);
    sys::path::remove_filename(scope);
    sys::path::append(scope, "patch-gc.lock");
    naspeed::SharedFileLock scopeLock;
    auto scopeResult = scopeLock.lock(scope.c_str());
    if (scopeResult == naspeed::FileLockResult::Failed) {
      errs() << "naspeed lazy run: cannot lock app patch cache: "
             << std::strerror(errno) << '\n';
      return 125;
    }
    auto opened = MemoryBuffer::getFile(argv[1]);
    if (!opened) {
      if (scopeResult == naspeed::FileLockResult::Locked &&
          opened.getError().value() == ENOENT)
        errs() << "naspeed lazy run: app patch generation expired; launch "
                  "manifest is no longer available\n";
      else
        errs() << "naspeed lazy run: cannot read launch manifest: "
               << opened.getError().message() << '\n';
      return 125;
    }
    manifest = std::move(*opened);
    manifest->getBuffer().split(fields, '\0', -1, false);

    SmallString<256> lease(generation);
    sys::path::append(lease, "patch-lease");
    if (generationLease.lock(lease.c_str()) ==
        naspeed::FileLockResult::Failed) {
      errs() << "naspeed lazy run: cannot lease app patch generation: "
             << std::strerror(errno) << '\n';
      return 125;
    }
    scopeLock.unlock();
  }
  bool hasImage = fields.size() >= 4 && fields[0] == "naspeed-demand-image-v1";
  bool appImage = fields.size() >= 3 && fields[0] == "naspeed-app-image-v1";
  bool cargoDemand = appImage || hasImage ||
                     (fields.size() >= 3 && fields[0] == "naspeed-demand-v1");
  if (appImage && !naspeed::runAppPatch(fields[1], report, argc - 2, argv + 2))
    naspeed::runAppImage(fields[1], report, argc - 2, argv + 2);
  InitializeNativeTarget();
  InitializeNativeTargetAsmPrinter();
  auto target = check(JITTargetMachineBuilder::detectHost());
  target.setCodeGenOptLevel(CodeGenOptLevel::None);
  SmallString<256> cache;
  if (!demand && !cargoDemand) {
    cache = argv[1];
    sys::path::remove_filename(cache);
    sys::path::remove_filename(cache);
    sys::path::append(cache, "objects");
    if (sys::fs::create_directory(cache))
      cache.clear(); // A read-only target can run without a disk cache.
  }
  auto targetKey = (Twine("naspeed-orc-object-v1 " LLVM_VERSION_STRING " ") +
                    target.getTargetTriple().str() + " " + target.getCPU() +
                    " " + target.getFeatures().getString())
                       .str();
  auto jit = check(LLLazyJITBuilder()
                       .setJITTargetMachineBuilder(std::move(target))
                       .setCompileFunctionCreator(
                           [&](JITTargetMachineBuilder builder)
                               -> Expected<std::unique_ptr<IRCompileLayer::IRCompiler>> {
                             auto tm = builder.createTargetMachine();
                             if (!tm)
                               return tm.takeError();
                             return std::make_unique<CachedCompiler>(
                                 std::move(*tm), cache,
                                 cache.empty() ? std::string() : targetKey, report);
                           })
                       .setLazyCompileFailureAddr(
                           ExecutorAddr::fromPtr(&materializationFailed))
                       .create());
  jit->getExecutionSession().setErrorReporter([](Error err) {
    logAllUnhandledErrors(std::move(err), errs(), "naspeed lazy run: ");
  });
  if (demand)
    naspeed::runDemandGuest(*jit, ExecutorAddr::fromPtr(&materializationFailed),
                           {}, {}, argv[2], {}, {}, {}, report, argc - 3, argv + 3);

  if (cargoDemand) {
    SmallString<256> driver(argv[0]);
    sys::path::remove_filename(driver);
    sys::path::append(driver, "naspeed-rustc-demand");
    naspeed::runDemandGuest(*jit, ExecutorAddr::fromPtr(&materializationFailed),
                           driver, fields[1], fields[2],
                           ArrayRef<StringRef>(fields).drop_front(hasImage ? 4 : 3),
                           hasImage ? fields[3] : StringRef(), targetKey,
                           report, argc - 2, argv + 2);
  }
  if (fields.size() < 3 || fields[0] != "naspeed-orc-v1") {
    errs() << "naspeed lazy run: invalid launch manifest\n";
    return 125;
  }
  jit->setPartitionFunction(IRPartitionLayer::compileRequested);
  auto &guest = jit->getMainJITDylib();
  auto &std = check(jit->loadPlatformDynamicLibrary(fields[2].str().c_str()));
  guest.addToLinkOrder(std);
  check(jit->linkStaticLibraryInto(guest, NASPEED_CLANG_BUILTINS));
  for (auto archive : ArrayRef<StringRef>(fields).drop_front(3)) {
    StringRef path = archive;
    if (path.ends_with(".dylib")) {
      auto &loaded =
          check(jit->loadPlatformDynamicLibrary(path.str().c_str()));
      guest.addToLinkOrder(loaded);
    } else {
      check(jit->linkStaticLibraryInto(guest, path.str().c_str()));
    }
  }

  if (report) {
    jit->getIRCompileLayer().setNotifyCompiled(
        [](MaterializationResponsibility &, ThreadSafeModule module) {
          module.withModuleDo([](Module &m) {
            for (const auto &f : m)
              if (!f.isDeclaration())
                errs() << "naspeed lazy materialized: " << f.getName() << '\n';
          });
        });
  }

  auto context = std::make_unique<LLVMContext>();
  SMDiagnostic diagnostic;
  auto module = parseIRFile(fields[1], diagnostic, *context);
  if (!module) {
    diagnostic.print("naspeed lazy run", errs());
    return 125;
  }
  const auto *main = module->getFunction("main");
  if (module->getTargetTriple().getArch() != Triple::aarch64 ||
      !module->getTargetTriple().isOSDarwin() || !main ||
      main->isDeclaration() || main->getCallingConv() != CallingConv::C ||
      main->isVarArg() || !main->getReturnType()->isIntegerTy(32) ||
      main->arg_size() != 2 || !main->getArg(0)->getType()->isIntegerTy(32) ||
      !main->getArg(1)->getType()->isPointerTy()) {
    errs() << "naspeed lazy run: bitcode has no Darwin arm64 C main\n";
    return 125;
  }
  if (!module->getModuleInlineAsm().empty() ||
      llvm::any_of(module->globals(), [](const GlobalVariable &g) {
        return g.hasSection() && g.getSection().contains("__mod_init_func");
      })) {
    errs() << "naspeed lazy run: native assembly and Mach-O constructors "
              "are unsupported\n";
    return 125;
  }
  // The artifact path includes a generation nonce. It is not a code input.
  module->setModuleIdentifier("naspeed-app");
  // CompileOnDemandLayer assigns stubs before IRPartitionLayer promotes
  // locals. Promote Rust's internal functions now, otherwise an internal
  // call resolves straight to the implementation and compiles eagerly.
  for (auto &f : *module)
    if (!f.isDeclaration() && f.hasLocalLinkage()) {
      f.setName(Twine("__naspeed_local.") + f.getName());
      f.setLinkage(GlobalValue::ExternalLinkage);
      f.setVisibility(GlobalValue::HiddenVisibility);
    }
  // Rust std on Darwin reads the CRT's argv, not the parameters to C main.
  *_NSGetArgc() = argc - 2;
  *_NSGetArgv() = argv + 2;
  setprogname(argv[2]);
  check(jit->addLazyIRModule(ThreadSafeModule(std::move(module), std::move(context))));
  check(jit->initialize(guest));
  auto entry = check(jit->lookup("main")).toPtr<int(int, char **)>();
  int status = entry(argc - 2, argv + 2);
  check(jit->deinitialize(guest));
  // Guest atexit callbacks may refer to JIT code, so do not unmap it before exit.
  std::exit(status);
}

} // namespace

// `ns patch` compiles nasc-emitted impl.ll files to Mach-O objects without
// spinning up clang: the JIT's own target machine already knows arm64.
static int emitObj(int argc, char **argv) {
  if (argc != 4) {
    errs() << "usage: naspeed-lazy-launch --emit-obj IMPL.ll OUT.o\n";
    return 125;
  }
  InitializeNativeTarget();
  InitializeNativeTargetAsmPrinter();
  LLVMContext context;
  SMDiagnostic diagnostics;
  auto module = parseIRFile(argv[2], diagnostics, context);
  if (!module) {
    diagnostics.print("naspeed emit-obj", errs());
    return 125;
  }
  auto builder = orc::JITTargetMachineBuilder::detectHost();
  if (!builder) {
    consumeError(builder.takeError());
    return 125;
  }
  auto machine = builder->createTargetMachine();
  if (!machine) {
    consumeError(machine.takeError());
    return 125;
  }
  if (auto error = naspeed::emit(*module, **machine, argv[3])) {
    logAllUnhandledErrors(std::move(error), errs(), "naspeed emit-obj: ");
    return 125;
  }
  return 0;
}

int main(int argc, char **argv) {
  if (argc >= 2 && std::strcmp(argv[1], "--emit-obj") == 0)
    return emitObj(argc, argv);
  if (argc >= 2 && std::strcmp(argv[1], "--app-patch-prepare") == 0)
    return naspeed::prepareAppPatch(argc, argv);
  if (argc < 3) {
    errs() << "usage: naspeed-lazy-launch MANIFEST GUEST-ARGV0 [ARGS...]\n";
    return 125;
  }
  return run(argc, argv);
}
