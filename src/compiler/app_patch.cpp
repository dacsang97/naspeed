#include "llvm/ADT/SmallPtrSet.h"
#include "llvm/ADT/SmallString.h"
#include "llvm/ADT/STLExtras.h"
#include "llvm/ADT/StringExtras.h"
#include "llvm/Bitcode/BitcodeWriter.h"
#include "llvm/Demangle/Demangle.h"
#include "llvm/ExecutionEngine/Orc/JITTargetMachineBuilder.h"
#include "llvm/IR/DebugInfo.h"
#include "llvm/IR/IRBuilder.h"
#include "llvm/IR/InlineAsm.h"
#include "llvm/IR/LegacyPassManager.h"
#include "llvm/IR/Verifier.h"
#include "llvm/IRReader/IRReader.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Object/MachO.h"
#include "llvm/Object/SymbolicFile.h"
#include "llvm/Support/Path.h"
#include "llvm/Support/SHA256.h"
#include "llvm/Support/SourceMgr.h"
#include "llvm/Support/TargetSelect.h"
#include "llvm/Target/TargetMachine.h"
#include "llvm/Transforms/Utils/Cloning.h"

#include <cstdlib>
#include <map>
#include <set>

using namespace llvm;

namespace naspeed {
Error emit(Module &module, TargetMachine &target, StringRef output);
}

namespace {

constexpr StringLiteral prefix("__ns_patch_");

Error refuse(const Twine &reason) {
  return createStringError(inconvertibleErrorCode(), reason);
}

std::string path(StringRef directory, StringRef member, StringRef extension = {}) {
  SmallString<256> result(directory);
  sys::path::append(result, member);
  result += extension;
  return result.str().str();
}

bool eligible(const Function &f) {
  return !f.isDeclaration() && !f.isVarArg() && !f.hasComdat() &&
         !f.hasSection() && !f.hasPrefixData() && !f.hasPrologueData() &&
         !f.hasGC() && f.getAddressSpace() == 0 &&
         (f.hasLocalLinkage() || f.hasExternalLinkage()) &&
         !f.hasFnAttribute(Attribute::Naked) &&
         !f.hasFnAttribute(Attribute::ReturnsTwice) &&
         !f.hasFnAttribute(Attribute::AlwaysInline) &&
         llvm::none_of(f.users(), [](const User *user) { return isa<BlockAddress>(user); });
}

bool references(const Value *operand, SmallPtrSetImpl<const Value *> &seen,
                SmallPtrSetImpl<GlobalValue *> &refs) {
  if (!seen.insert(operand).second)
    return true;
  if (auto *assembly = dyn_cast<InlineAsm>(operand))
    // This admits the empty compiler barrier emitted by black_box, not
    // arbitrary assembler references LLVM cannot enumerate.
    return assembly->getAsmString().empty();
  if (auto *g = dyn_cast<GlobalValue>(operand)) {
    if (auto *f = dyn_cast<Function>(g); f && f->isIntrinsic())
      return true;
    if ((!isa<Function>(g) && !isa<GlobalVariable>(g)) ||
        g->getName().empty() || g->hasAppendingLinkage() ||
        g->hasAvailableExternallyLinkage() || g->isDeclarationForLinker() != g->isDeclaration())
      return false;
    refs.insert(const_cast<GlobalValue *>(g));
    return true;
  }
  if (isa<BlockAddress, MetadataAsValue>(operand))
    return false;
  if (auto *constant = dyn_cast<Constant>(operand))
    for (const auto &arg : constant->operands())
      if (!references(arg.get(), seen, refs))
        return false;
  return true;
}

void forward(Module &module, Function &body, unsigned index) {
  auto name = body.getName().str();
  body.setName(Twine(prefix) + "body_" + Twine(index));
  // Callers live in sibling partition objects, so the stub must be
  // visible outside its own object even when the body was local.
  auto *stub = Function::Create(body.getFunctionType(), GlobalValue::ExternalLinkage,
                                name, &module);
  stub->copyAttributesFrom(&body);
  stub->removeFnAttr(Attribute::Speculatable);
  stub->removeFnAttr(Attribute::Memory);
  stub->removeFnAttr(Attribute::NoSync);
  body.replaceAllUsesWith(stub);
  body.setLinkage(GlobalValue::ExternalLinkage);
  body.setVisibility(GlobalValue::DefaultVisibility);
  body.setDSOLocal(false);
  auto *ptr = PointerType::getUnqual(module.getContext());
  // The dispatch record lives in its own partition; the stub only needs a
  // declaration here.
  auto *table = cast<GlobalVariable>(
      module.getOrInsertGlobal("__ns_patch_table_v2", ptr));
  auto *gate = cast<GlobalVariable>(module.getOrInsertGlobal(
      "__ns_patch_gate_v2", Type::getInt64Ty(module.getContext())));
  auto *dispatchType = StructType::get(Type::getInt64Ty(module.getContext()), ptr);
  IRBuilder<> builder(BasicBlock::Create(module.getContext(), "", stub));
  SmallVector<Value *, 8> args;
  for (auto &arg : stub->args())
    args.push_back(&arg);
  auto *one = builder.getInt64(1);
  // Pin the dispatch record before following its function pointer. The
  // gate covers the interval in which a caller can see the table but has
  // not incremented its per-revision count yet.
  builder.CreateAtomicRMW(AtomicRMWInst::Add, gate, one, Align(8),
                          AtomicOrdering::SequentiallyConsistent);
  auto *revision = builder.CreateLoad(ptr, table);
  revision->setAtomic(AtomicOrdering::SequentiallyConsistent);
  revision->setAlignment(Align(8));
  auto *readers = builder.CreateStructGEP(dispatchType, revision, 0);
  builder.CreateAtomicRMW(AtomicRMWInst::Add, readers, one, Align(8),
                          AtomicOrdering::Monotonic);
  builder.CreateAtomicRMW(AtomicRMWInst::Sub, gate, one, Align(8),
                          AtomicOrdering::SequentiallyConsistent);
  auto *functions = builder.CreateLoad(ptr, builder.CreateStructGEP(dispatchType, revision, 1));
  auto *slot = builder.CreateInBoundsGEP(ptr, functions, builder.getInt32(index));
  auto *procedure = builder.CreateLoad(ptr, slot);
  auto release = [&] {
    builder.CreateAtomicRMW(AtomicRMWInst::Sub, readers, one, Align(8),
                            AtomicOrdering::Release);
  };

  CallBase *call;
  if (body.doesNotThrow()) {
    call = builder.CreateCall(body.getFunctionType(), procedure, args);
  } else {
    auto *normal = BasicBlock::Create(module.getContext(), "", stub);
    auto *cleanup = BasicBlock::Create(module.getContext(), "", stub);
    if (body.hasPersonalityFn())
      stub->setPersonalityFn(body.getPersonalityFn());
    else {
      auto *type = FunctionType::get(builder.getInt32Ty(), true);
      auto personality = module.getOrInsertFunction("rust_eh_personality", type);
      stub->setPersonalityFn(cast<Constant>(personality.getCallee()));
    }
    call = builder.CreateInvoke(body.getFunctionType(), procedure, normal, cleanup, args);
    builder.SetInsertPoint(cleanup);
    auto *landing = builder.CreateLandingPad(StructType::get(ptr, builder.getInt32Ty()), 0);
    landing->setCleanup(true);
    release();
    builder.CreateResume(landing);
    builder.SetInsertPoint(normal);
  }
  call->setCallingConv(body.getCallingConv());
  call->setAttributes(body.getAttributes());
  release();
  if (body.getReturnType()->isVoidTy())
    builder.CreateRetVoid();
  else
    builder.CreateRet(call);
}

// The bitcode writer already buffers its output. Feed it directly to
// SHA-256 instead of storing a second copy of the partition.
class BitcodeHash final : public raw_ostream {
  SHA256 &digest;
  uint64_t size = 0;
  void write_impl(const char *data, size_t count) override {
    digest.update(StringRef(data, count));
    size += count;
  }
  uint64_t current_pos() const override { return size; }

public:
  explicit BitcodeHash(SHA256 &digest) : raw_ostream(true), digest(digest) {}
};

std::string key(Module &module, StringRef base = {},
                const std::set<std::string> *exports = nullptr,
                const std::set<std::string> *imports = nullptr) {
  SHA256 digest;
  digest.update(base);
  // Hash the printed IR, not bitcode: the writer embeds use-list and
  // operand ordering that varies between runs over identical IR.
  {
    std::string text;
    raw_string_ostream os(text);
    module.print(os, nullptr);
    digest.update(text);
  }
  // Export/import policy is a linker input even when the LLVM linkage
  // and visibility of a newly referenced base symbol are already correct.
  for (const auto *symbols : {exports, imports}) {
    if (symbols)
      for (const auto &name : *symbols) {
        digest.update(name);
        digest.update(StringRef("\0", 1));
      }
    digest.update(StringRef("\0", 1));
  }
  return toHex(digest.final(), true);
}

bool cached(StringRef cache, StringRef key) {
  auto directory = path(cache, key);
  return sys::fs::is_regular_file(path(directory, "app.dylib")) &&
         sys::fs::is_regular_file(path(directory, "app.sha256"));
}

Error writeSymbols(StringRef output, const std::set<std::string> &symbols) {
  std::error_code error;
  raw_fd_ostream file(output, error);
  if (error)
    return errorCodeToError(error);
  for (const auto &symbol : symbols)
    file << symbol << '\n';
  file.close();
  return errorCodeToError(file.error());
}


std::string digestFile(StringRef path) {
  auto file = MemoryBuffer::getFile(path);
  if (!file)
    return {};
  SHA256 digest;
  digest.update((*file)->getBuffer());
  return toHex(digest.final(), true);
}

struct Unit {
  std::string bcPath;
  std::string objPath;
  std::string objectKey;
  SmallPtrSet<const Function *, 4> selectedBodies;
};

/// Defined global and undefined symbols of one Mach-O object. Symbol
/// names keep the Mach-O leading underscore; the IR name drops one.
struct Scan {
  std::set<std::string> defined;
  std::set<std::string> undefined;
  // All defined names, including locals; selector ownership must see
  // internal-linkage functions that never appear in the global table.
  std::set<std::string> all;
};

// `ns patch` and repeated `ns build --lazy-ir` runs rescan the same CGU
// objects; the sets are deterministic, so a content-keyed record lets
// prepare skip the Mach-O walk entirely for unchanged units.
static bool loadScan(StringRef cache, StringRef key, Scan &scan) {
  SmallString<256> path(cache);
  sys::path::append(path, "scan-" + key);
  auto file = MemoryBuffer::getFile(path);
  if (!file)
    return false;
  SmallVector<StringRef, 64> lines;
  (*file)->getBuffer().split(lines, '\n', -1, false);
  for (StringRef line : lines) {
    if (line.size() < 3 || line[1] != ' ')
      continue;
    StringRef name = line.drop_front(2);
    if (line[0] == 'd')
      scan.defined.insert(name.str());
    else if (line[0] == 'u')
      scan.undefined.insert(name.str());
    else if (line[0] == 'a')
      scan.all.insert(name.str());
  }
  return true;
}

static void storeScan(StringRef cache, StringRef key, const Scan &scan) {
  SmallString<256> path(cache);
  sys::path::append(path, "scan-" + key);
  SmallString<256> stage(cache);
  sys::path::append(stage, "scan-stage");
  std::error_code error;
  raw_fd_ostream out(stage, error, sys::fs::OF_None);
  if (error)
    return;
  for (const auto &name : scan.defined)
    out << "d " << name << '\n';
  for (const auto &name : scan.undefined)
    out << "u " << name << '\n';
  for (const auto &name : scan.all)
    out << "a " << name << '\n';
  out.close();
  if (out.has_error())
    return;
  sys::fs::rename(stage, path);
}

// The base link only needs to know "did this unit change"; cargo already
// invalidates on mtime, so a path+size+mtime fingerprint is as honest as a
// full digest and skips hashing hundreds of megabytes of bitcode per build.
static std::string statKey(StringRef path) {
  sys::fs::file_status status;
  if (sys::fs::status(path, status))
    return {};
  auto nanos = status.getLastModificationTime().time_since_epoch().count();
  return (Twine(status.getSize()) + Twine('-') + Twine(nanos)).str();
}

static std::string digestText(StringRef text) {
  SHA256 hash;
  hash.update(text);
  return toHex(hash.final(), true);
}

static Error scanObject(StringRef objPath, Scan &scan) {
  auto buffer = MemoryBuffer::getFile(objPath);
  if (!buffer)
    return refuse(Twine("could not read object: ") + objPath);
  auto objectOr = object::MachOObjectFile::create((*buffer)->getMemBufferRef(),
                                                  /*IsLittleEndian*/ true,
                                                  /*Is64Bits*/ true);
  if (!objectOr) {
    consumeError(objectOr.takeError());
    return refuse(Twine("unsupported object format: ") + objPath);
  }
  auto &objectFile = **objectOr;
  for (auto &symbol : objectFile.symbols()) {
    auto nameOr = symbol.getName();
    if (!nameOr)
      continue;
    auto flagsOr = symbol.getFlags();
    if (!flagsOr)
      continue;
    auto flags = *flagsOr;
    if (!(flags & object::SymbolRef::SF_Undefined))
      scan.all.insert(nameOr->str());
    if (!(flags & object::SymbolRef::SF_Global))
      continue;
    if (flags & object::SymbolRef::SF_Undefined)
      scan.undefined.insert(nameOr->str());
    else
      scan.defined.insert(nameOr->str());
  }
  return Error::success();
}

// The input is a manifest of per-CGU bitcode files. rustc already emitted
// one object per unit and reuses unchanged units across edits, so prepare
// only re-emits units that gain a forward stub, plus the shared dispatch
// record and the patch implementations.
Error prepare(StringRef manifest, StringRef work, StringRef cache, StringRef selectors) {
  LLVMContext context;
  auto list = MemoryBuffer::getFile(manifest);
  if (!list)
    return refuse("could not read bitcode manifest");
  SmallVector<StringRef, 8> names;
  (*list)->getBuffer().split(names, '\n', -1, false);
  if (names.empty())
    return refuse("empty bitcode manifest");
  SmallVector<StringRef, 4> wanted;
  selectors.split(wanted, ',', -1, true);
  if (wanted.empty() || wanted.size() > 64)
    return refuse("expected 1–64 function names");

  // rustc emits <name>.rcgu.bc next to <name>.rcgu.o; the object is
  // reused by rustc's own incremental cache when the unit is unchanged.
  SmallVector<Unit, 8> units;
  SmallVector<Scan, 8> scans;
  std::set<std::string> allDefined{"_main", "___ns_patch_table_v2",
                                   "___ns_patch_gate_v2"},
      allUndefined;
  for (auto name : names) {
    Unit unit;
    unit.bcPath = name.str();
    unit.objPath = name.str();
    if (!StringRef(unit.objPath).ends_with(".bc"))
      return refuse(Twine("unexpected bitcode name: ") + name);
    unit.objPath.resize(unit.objPath.size() - 2);
    unit.objPath += 'o';
    Scan scan;
    {
      std::string key = statKey(unit.objPath);
      if (key.empty())
        return refuse(Twine("could not stat application object: ") + name);
      std::string objKey = digestText(unit.objPath + ":" + key);
      if (!loadScan(cache, objKey, scan)) {
        if (auto error = scanObject(unit.objPath, scan))
          return error;
        storeScan(cache, objKey, scan);
      }
    }
    allDefined.insert(scan.defined.begin(), scan.defined.end());
    allUndefined.insert(scan.undefined.begin(), scan.undefined.end());
    unit.objectKey = statKey(unit.bcPath);
    if (unit.objectKey.empty())
      return refuse(Twine("could not stat application bitcode: ") + name);
    units.push_back(std::move(unit));
    scans.push_back(std::move(scan));
  }
  if (!allDefined.count("_main"))
    return refuse("unsupported application module: no main");

  // Selector resolution runs on the Mach-O defined table: a Rust leaf name
  // has exactly one owning unit. IR names drop the Mach-O underscore.
  SmallVector<Function *, 4> selected(wanted.size(), nullptr);
  std::map<std::pair<unsigned, unsigned>, StringRef> owners;
  for (unsigned u = 0; u < scans.size(); ++u) {
    for (const auto &defined : scans[u].all) {
      if (!StringRef(defined).starts_with("__R"))
        continue;
      std::unique_ptr<char, decltype(&std::free)> demangled(
          rustDemangle(StringRef(defined).drop_front()), std::free);
      if (!demangled)
        continue;
      auto leaf = StringRef(demangled.get()).rsplit("::").second;
      for (size_t i = 0; i < wanted.size(); ++i)
        if (wanted[i] == leaf)
          owners[{u, i}] = defined;
    }
  }
  for (unsigned i = 0; i < wanted.size(); ++i) {
    unsigned count = 0;
    for (unsigned u = 0; u < scans.size(); ++u)
      count += owners.count({u, i});
    if (count != 1)
      return refuse(Twine("Rust function not found (expected a unique v0 symbol): ") + wanted[i]);
  }

  // Only units that own a selected function need their bitcode parsed:
  // the stub mutation, patch clone, and patch table references live there.
  std::set<unsigned> stubbedUnits;
  for (const auto &[pair, symbol] : owners)
    stubbedUnits.insert(pair.first);
  std::map<unsigned, std::unique_ptr<Module>> stubbedModules;
  SmallPtrSet<const Function *, 8> bodies;
  std::set<std::string> exports, imports;
  SmallPtrSet<const Value *, 32> seen;
  SmallPtrSet<GlobalValue *, 32> refs;
  for (unsigned u : stubbedUnits) {
    SMDiagnostic diagnostics;
    auto module = parseIRFile(units[u].bcPath, diagnostics, context);
    if (!module) {
      diagnostics.print("naspeed app patch", errs());
      return refuse(Twine("could not read application bitcode: ") + units[u].bcPath);
    }
    Triple triple(module->getTargetTriple());
    if (!triple.isMacOSX() || triple.getArch() != Triple::aarch64 ||
        !module->getModuleInlineAsm().empty() ||
        module->getNamedValue("llvm.global_ctors") ||
        module->getNamedValue("llvm.global_dtors"))
      return refuse(Twine("unsupported application module: ") + units[u].bcPath);
    StripDebugInfo(*module);
    for (const auto &g : module->global_values())
      if (g.getName().starts_with(prefix))
        return refuse("reserved patch symbol name");
    for (unsigned i = 0; i < wanted.size(); ++i) {
      auto owner = owners.find({u, i});
      if (owner == owners.end())
        continue;
      auto *f = module->getFunction(StringRef(owner->second).drop_front());
      if (!f || f->isDeclaration() || !eligible(*f) || !bodies.insert(f).second)
        return refuse(Twine("ambiguous or unsupported Rust function: ") + wanted[i]);
      selected[i] = f;
      units[u].selectedBodies.insert(f);
      forward(*module, *f, i);
      for (auto &block : *f)
        for (auto &instruction : block)
          for (auto &arg : instruction.operands())
            if (!references(arg.get(), seen, refs))
              return refuse("unrepresentable instruction dependency");
      for (auto &arg : f->operands())
        if (!references(arg.get(), seen, refs))
          return refuse("unrepresentable function dependency");
    }
    for (auto &g : module->global_values())
      if (!g.isDeclaration() && !g.hasLocalLinkage() &&
          !g.getName().starts_with("llvm."))
        exports.insert((Twine("_") + g.getName()).str());
    // The module identifier embeds the .bc path with rustc's per-build
    // CGU hash suffix; keys must be content only, so normalize it away.
    module->setModuleIdentifier("naspeed-unit");
    module->setSourceFileName("naspeed-unit");
    stubbedModules[u] = std::move(module);
  }
  exports.insert(allDefined.begin(), allDefined.end());
  // Only undefined symbols a patch body references need the -u archive
  // hint; the base's own undefineds are already in the link.
  std::set<std::string> undefinedRefs;
  for (auto *ref : refs) {
    auto name = ref->getName();
    if (name.starts_with("\1") || name.contains('\n') || name.contains('\r'))
      return refuse("unsupported Mach-O symbol spelling");
    std::string symbol = (Twine("_") + name).str();
    if (ref->isDeclaration()) {
      undefinedRefs.insert(symbol);
    } else {
      // The patch clone references this definition as an external
      // declaration, so the stubbed unit must export it. Hidden or
      // local linkage is promoted in place before the unit is emitted.
      ref->setLinkage(GlobalValue::ExternalLinkage);
      ref->setVisibility(GlobalValue::DefaultVisibility);
      ref->setDSOLocal(false);
      exports.insert(symbol);
    }
  }

  // A referenced declaration may be defined hidden (private extern) or
  // local in another codegen unit; such units are re-emitted with the
  // symbol promoted so the base image can export it. Definitions living
  // only in archives are pulled in with -u instead.
  std::set<unsigned> promotedUnits;
  for (const auto &symbol : undefinedRefs) {
    bool definedInUnit = false;
    for (unsigned u = 0; u < scans.size(); ++u) {
      if (!scans[u].defined.count(symbol))
        continue;
      definedInUnit = true;
      if (!stubbedUnits.count(u))
        promotedUnits.insert(u);
    }
    if (!definedInUnit) {
      imports.insert(symbol);
      exports.insert(symbol);
    }
  }
  for (unsigned u : promotedUnits) {
    SMDiagnostic diagnostics;
    auto module = parseIRFile(units[u].bcPath, diagnostics, context);
    if (!module) {
      diagnostics.print("naspeed app patch", errs());
      return refuse(Twine("could not read application bitcode: ") + units[u].bcPath);
    }
    for (const auto &symbol : undefinedRefs) {
      if (!scans[u].defined.count(symbol))
        continue;
      if (auto *gv = module->getNamedValue(StringRef(symbol).drop_front())) {
        gv->setLinkage(GlobalValue::ExternalLinkage);
        gv->setVisibility(GlobalValue::DefaultVisibility);
        gv->setDSOLocal(false);
      }
    }
    module->setModuleIdentifier("naspeed-unit");
    module->setSourceFileName("naspeed-unit");
    stubbedModules[u] = std::move(module);
    stubbedUnits.insert(u);
  }

  // The shared dispatch record is its own partition so stubs in any unit
  // can declare it.
  auto dispatch = std::make_unique<Module>("naspeed-dispatch", context);
  {
    auto &home = stubbedModules.begin()->second;
    dispatch->setTargetTriple(home->getTargetTriple());
    dispatch->setDataLayout(home->getDataLayout());
  }
  auto *ptr = PointerType::getUnqual(context);
  auto *counter = Type::getInt64Ty(context);
  auto *table = new GlobalVariable(*dispatch, ptr, false, GlobalValue::ExternalLinkage,
                                   ConstantPointerNull::get(ptr), "__ns_patch_table_v2");
  auto *gate = new GlobalVariable(*dispatch, counter, false, GlobalValue::ExternalLinkage,
                                  ConstantInt::get(counter, 0), "__ns_patch_gate_v2");
  table->setAlignment(Align(8));
  gate->setAlignment(Align(8));
  auto dispatchKey = key(*dispatch);

  // naspeed's own compiler (nasc): parse the selected body from Rust
  // source and emit the patch impl as LLVM IR text, skipping the
  // rustc-bitcode clone. Only runs when a `nasc` sibling binary exists
  // and the crate's root source path reached us via NASPEED_NASC_SOURCE.
  std::map<size_t, std::string> nascImpls;
  if (!std::getenv("NASPEED_DISABLE_NASC")) {
    SmallString<256> nasc(sys::fs::getMainExecutable(nullptr, nullptr));
    sys::path::remove_filename(nasc);
    sys::path::append(nasc, "nasc");
    const char *srcfile = std::getenv("NASPEED_NASC_SOURCE");
    if (sys::fs::is_regular_file(nasc) && srcfile && *srcfile &&
        sys::fs::is_regular_file(srcfile)) {
      bool any = false;
      for (size_t i = 0; i < selected.size(); ++i) {
        auto *body = selected[i];
        const Module *home = nullptr;
        for (auto &[u, module] : stubbedModules)
          if (units[u].selectedBodies.contains(body))
            home = module.get();
        if (!home)
          continue;
        std::error_code ec;
        std::string sigpath = path(work, (Twine("nasc-sig-") + Twine(i)).str(), ".txt");
        raw_fd_ostream out(sigpath, ec);
        if (ec)
          return errorCodeToError(ec);
        out << "triple " << home->getTargetTriple().str() << '\n';
        out << "layout " << home->getDataLayoutStr() << '\n';
        out << "want " << wanted[i] << '\n';
        // `define` carries the real impl name; nasc prints it verbatim.
        out << "define ";
        body->getReturnType()->print(out, false);
        out << " @" << prefix << "impl_" << i << "(";
        bool first = true;
        for (auto &arg : body->args()) {
          if (!first)
            out << ", ";
          first = false;
          arg.getType()->print(out, false);
        }
        out << ")\n";
        // Every callable visible to the patch: definitions in the base
        // (stub units) plus the imports it already links.
        for (auto &[u, module] : stubbedModules)
          for (auto &f : *module)
            if (!f.getName().starts_with(prefix) &&
                !f.getName().starts_with("llvm.")) {
              out << "decl @" << f.getName() << ' ';
              f.getReturnType()->print(out, false);
              std::string mangled = f.getName().str();
              if (!mangled.empty() && mangled.front() == '_')
                mangled.erase(mangled.begin());
              std::string dem = demangle(mangled);
              std::string leaf =
                  dem.substr(dem.rfind(':') == std::string::npos
                                 ? 0
                                 : dem.rfind(':') + 1);
              out << ' ' << (leaf == dem ? "-" : leaf) << " |";
              first = true;
              for (auto &arg : f.args()) {
                if (!first)
                  out << ", ";
                first = false;
                arg.getType()->print(out, false);
              }
              out << '\n';
            }
        // Verbatim call sites from the original body: the strongest
        // evidence for reissuing calls — exact callee symbol, sret /
        // align / ext attributes preserved, closure args never appear.
        for (auto &bb : *body) {
          for (auto &ins : bb) {
            if (auto *cb = dyn_cast<CallBase>(&ins)) {
              Function *f = cb->getCalledFunction();
              if (!f || f->getName().starts_with(prefix) ||
                  f->getName().starts_with("llvm."))
                continue;
              out << "call " << f->getName() << " |";
              {
                std::string mangled = f->getName().str();
                if (!mangled.empty() && mangled.front() == '_')
                  mangled.erase(mangled.begin());
                std::string dem = demangle(mangled);
                out << ' ' << (dem == mangled ? "-" : dem) << " |";
              }
              if (cb->hasRetAttr(Attribute::ZExt))
                out << "zeroext ";
              else if (cb->hasRetAttr(Attribute::SExt))
                out << "signext ";
              f->getReturnType()->print(out, false);
              out << " |";
              for (unsigned a = 0; a < cb->arg_size(); ++a) {
                if (a)
                  out << ";";
                cb->getArgOperand(a)->getType()->print(out, false);
                if (cb->paramHasAttr(a, Attribute::StructRet)) {
                  out << " sret(";
                  cb->getParamStructRetType(a)->print(out, false);
                  out << ")";
                }
                if (auto al = cb->getParamAlign(a))
                  out << " align " << al->value();
              }
              out << '\n';
            }
          }
        }
        out.close();
        if (auto ec2 = out.error())
          return errorCodeToError(ec2);
        any = true;
      }
      if (any) {
        // One nasc pass compiles every selected body; per-function
        // failures simply leave no impl-<i>.ll behind. The target's own
        // source file ranks first inside nasc so sibling bin targets
        // with a same-named function can't shadow it.
        {
          std::error_code cnec;
          raw_fd_ostream cn(path(work, "nasc-source", ".txt"), cnec);
          if (!cnec) {
            cn << srcfile << '\n';
            cn.close();
          }
        }
        SmallString<256> srcdir(srcfile);
        sys::path::remove_filename(srcdir);
        sys::path::remove_filename(srcdir);
        sys::path::append(srcdir, "src");
        std::string cmd = nasc.str().str() + " batch \"" + work.str() +
                          "\" \"" + srcdir.str().str() + "\"";
        if (!std::getenv("NASPEED_LAZY_DUMP_PATCH"))
          cmd += " 2>/dev/null";
        std::system(cmd.c_str());
        for (size_t i = 0; i < selected.size(); ++i) {
          std::string llpath =
              path(work, (Twine("impl-") + Twine(i)).str(), ".ll");
          if (sys::fs::is_regular_file(llpath))
            nascImpls[i] = llpath;
        }
      }
    }
  }

  // The patch clone needs the renamed body alive, but the stub object
  // must not carry it: callers only ever dispatch through the table, so
  // keeping it would rekey the base on every body edit. Clone first,
  // delete second.
  struct PatchWork {
    std::unique_ptr<Module> module;
    Function *impl;
    std::set<std::string> needs;
  };
  std::vector<PatchWork> patches;
  patches.reserve(selected.size());
  for (size_t i = 0; i < selected.size(); ++i) {
    auto *body = selected[i];
    const Module *home = nullptr;
    for (auto &[u, module] : stubbedModules)
      if (units[u].selectedBodies.contains(body))
        home = module.get();
    ValueToValueMapTy map;
    std::unique_ptr<Module> patch;
    Function *impl = nullptr;
    if (auto ll = nascImpls.find(i); ll != nascImpls.end()) {
      SMDiagnostic nascDiag;
      patch = parseIRFile(ll->second, nascDiag, context);
      if (patch)
        impl = patch->getFunction((prefix + "impl_" + Twine(i)).str());
    }
    if (!impl) {
      patch = CloneModule(*home, map,
                          [&](const GlobalValue *g) { return g == body; });
      impl = cast<Function>(map[body]);
    }
    impl->setName(Twine(prefix) + "impl_" + Twine(i));
    patch->setModuleIdentifier("naspeed-patch");
    patch->setSourceFileName("naspeed-patch");
    std::set<std::string> needs;
    for (auto &block : *impl)
      for (auto &instruction : block)
        for (auto &arg : instruction.operands()) {
          if (auto *g = dyn_cast<GlobalValue>(arg.get());
              g && !g->getName().empty())
            needs.insert(g->getName().str());
          else if (auto *c = dyn_cast<Constant>(arg.get()))
            for (const auto &sub : c->operands())
              if (auto *sg = dyn_cast<GlobalValue>(sub.get()))
                if (!sg->getName().empty())
                  needs.insert(sg->getName().str());
        }
    if (std::getenv("NASPEED_LAZY_DUMP_PATCH")) {
      std::string text;
      raw_string_ostream os(text);
      patch->print(os, nullptr);
      std::error_code ec;
      raw_fd_ostream out(path(work, (Twine("patch-dump-") + Twine(i)).str(), ".ll"), ec);
      if (ec)
        return errorCodeToError(ec);
      out << text;
      out.close();
      if (auto ec2 = out.error())
        return errorCodeToError(ec2);
    }
      {
        std::error_code ec2;
        raw_fd_ostream nl(path(work, (Twine("patch-needs-") + Twine(i)).str(), ".txt"), ec2);
        if (!ec2) {
          for (const auto &n : needs) nl << n << '\n';
          nl.close();
        }
      }
    patches.push_back({std::move(patch), impl, std::move(needs)});
  }
  for (unsigned u : stubbedUnits)
    for (const auto *body : units[u].selectedBodies)
      const_cast<Function *>(body)->eraseFromParent();
  for (size_t i = 0; i < selected.size(); ++i)
    exports.erase((Twine("_") + prefix + "body_" + Twine(i)).str());
  // A deleted body can orphan its private globals (string/byte pool
  // allocations, promoted constants); local linkage means nothing
  // outside the unit can reach them, so they are dead weight in both
  // the stub object and the unit key.
  for (unsigned u : stubbedUnits) {
    SmallVector<GlobalVariable *, 8> dead;
    for (auto &g : stubbedModules[u]->globals())
      if (g.hasLocalLinkage() && g.use_empty() &&
          !g.getName().starts_with("llvm."))
        dead.push_back(&g);
    for (auto *g : dead)
      g->eraseFromParent();
  }

  // Units whose module was mutated (stub or promoted defs) key on the
  // mutated content; unchanged units keep their bitcode digest.
  for (unsigned u : stubbedUnits) {
    units[u].objectKey = key(*stubbedModules[u]);
    units[u].objPath = path(work, units[u].objectKey, ".o");
    if (std::getenv("NASPEED_LAZY_DUMP_PATCH")) {
      std::string text;
      raw_string_ostream os(text);
      stubbedModules[u]->print(os, nullptr);
      std::error_code ec;
      raw_fd_ostream out(path(work, (Twine("stub-dump-") + Twine(u)).str(), ".ll"), ec);
      if (!ec) { out << text; out.close(); }
    }
  }
  std::set<std::string> sortedKeys{dispatchKey};
  for (auto &unit : units)
    sortedKeys.insert(unit.objectKey);
  SHA256 baseDigest;
  for (const auto &part : sortedKeys) {
    baseDigest.update(part);
    baseDigest.update(StringRef("\0", 1));
  }
  for (const auto *symbols : {&exports, &imports})
    for (const auto &name : *symbols) {
      baseDigest.update(name);
      baseDigest.update(StringRef("\0", 1));
    }
  auto baseKey = toHex(baseDigest.final(), true);
  if (std::getenv("NASPEED_LAZY_DUMP_PATCH")) {
    std::error_code ec;
    raw_fd_ostream dbg(path(work, "base-key", ".txt"), ec);
    if (!ec) {
      dbg << baseKey << '\n';
      for (const auto &k : sortedKeys) dbg << "k " << k << '\n';
      for (const auto &e : exports) dbg << "e " << e << '\n';
      for (const auto &im : imports) dbg << "i " << im << '\n';
      dbg.close();
    }
  }

  // Use the module's deployment triple; host detection would use the
  // build machine's OS version instead of the app's target.
  InitializeNativeTarget();
  InitializeNativeTargetAsmPrinter();
  orc::JITTargetMachineBuilder builder(Triple(dispatch->getTargetTriple()));
  builder.setRelocationModel(Reloc::PIC_).setCodeGenOptLevel(CodeGenOptLevel::None);
  std::unique_ptr<TargetMachine> target;
  auto object = [&](Module &part, StringRef partKey) -> Error {
    if (!target) {
      auto machine = builder.createTargetMachine();
      if (!machine)
        return machine.takeError();
      target = std::move(*machine);
    }
    return naspeed::emit(part, *target, path(work, partKey, ".o"));
  };
  if (!cached(cache, baseKey)) {
    if (auto error = writeSymbols(path(work, "base.exports"), exports))
      return error;
    if (auto error = writeSymbols(path(work, "base.imports"), imports))
      return error;
    if (auto error = object(*dispatch, dispatchKey))
      return error;
    for (unsigned u : stubbedUnits)
      if (auto error = object(*stubbedModules[u], units[u].objectKey))
        return error;
  }
  outs() << baseKey << '\n';
  if (!cached(cache, baseKey)) {
    // The base link consumes object paths only on a cache miss; on a hit
    // no objects were emitted into this fresh work directory.
    outs() << "o " << path(work, dispatchKey, ".o") << '\n';
    for (auto &unit : units)
      outs() << "o " << unit.objPath << '\n';
  }
  for (size_t i = 0; i < selected.size(); ++i) {
    // Key on the impl body and the external names it needs, not the
    // whole clone: sibling declarations are incidental to the patch.
    SHA256 patchDigest;
    {
      std::string bodyText;
      raw_string_ostream os(bodyText);
      os << patches[i].module->getTargetTriple().str() << '\n'
         << patches[i].module->getDataLayoutStr() << '\n';
      patches[i].impl->print(os);
      patchDigest.update(baseKey);
      patchDigest.update(bodyText);
      for (const auto &name : patches[i].needs) {
        patchDigest.update(name);
        patchDigest.update(StringRef("\0", 1));
      }
    }
    auto patchKey = toHex(patchDigest.final(), true);
    if (!cached(cache, patchKey))
      if (auto error = object(*patches[i].module, patchKey))
        return error;
    outs() << "p " << i << ' ' << patchKey << '\n';
  }
  return Error::success();
}


} // namespace

namespace naspeed {

Error emit(Module &module, TargetMachine &target, StringRef output) {
  if (verifyModule(module, &errs()))
    return refuse("invalid partitioned IR");
  std::error_code error;
  raw_fd_ostream file(output, error);
  if (error)
    return errorCodeToError(error);
  legacy::PassManager passes;
  if (target.addPassesToEmitFile(passes, file, nullptr, CodeGenFileType::ObjectFile))
    return refuse("target cannot emit Mach-O objects");
  passes.run(module);
  file.close();
  return errorCodeToError(file.error());
}

int prepareAppPatch(int argc, char **argv) {
  if (argc != 6)
    return 125;
  if (auto error = prepare(argv[2], argv[3], argv[4], argv[5])) {
    logAllUnhandledErrors(std::move(error), errs(), "naspeed app patch: ");
    return 125;
  }
  return 0;
}

} // namespace naspeed
