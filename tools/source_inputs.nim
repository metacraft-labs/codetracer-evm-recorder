## Complete real source-file inputs for recognized-format Cargo actions.
## No mocks or test filtering: filesystem membership is observed by the actual
## provider and every regular file is passed to the engine for content hashing.
import std/[algorithm, os, sets, strutils]
import repro_project_dsl

type SourceMember = tuple[kind: PathComponent, path: string, identity: string]

proc failSource(message: string) {.noreturn.} =
  raise newException(ValueError, "EVM source input census: " & message)

proc members(directory: string): seq[SourceMember] =
  if symlinkExists(directory) or not dirExists(directory):
    failSource("missing or symlink directory: " & directory)
  for kind, path in walkDir(directory):
    if kind notin {pcFile, pcDir} or symlinkExists(path):
      failSource("symlink or unsupported entry: " & path)
    # walkDir's pcFile also covers FIFO/socket/device entries on Unix.
    # lstat-backed metadata does not open their contents or block on a FIFO.
    let info = getFileInfo(path, followSymlink = false)
    if info.kind != kind or info.isSpecial:
      failSource("nonregular or changed-type entry: " & path)
    result.add((kind, path, $info.id))
  result.sort(proc(a, b: SourceMember): int = cmp(a.path, b.path))

proc completeRegularSourceInputs*(projectRoot: string;
                                  roots: openArray[string]): seq[string] =
  if projectRoot.len == 0:
    failSource("missing owning package root")
  let project = absolutePath(projectRoot).normalizedPath
  if symlinkExists(project) or not dirExists(project):
    failSource("invalid owning package root: " & project)
  let workspace = expandFilename(project.parentDir)
  var files: seq[string]
  var seen = initHashSet[string]()

  proc visit(directory, boundary: string) =
    if symlinkExists(directory) or not dirExists(directory):
      failSource("directory changed type: " & directory)
    let directoryBefore = getFileInfo(directory, followSymlink = false)
    if directoryBefore.kind != pcDir or directoryBefore.isSpecial:
      failSource("non-directory selected root: " & directory)
    # Register before consuming membership. The interface-mode no-op is not
    # qualification evidence; emitted provider observations must be verified.
    providerDirectoryInput(directory)
    let before = members(directory)
    if members(directory) != before:
      failSource("membership changed during provider registration: " & directory)
    for entry in before:
      if symlinkExists(entry.path):
        failSource("entry became a symlink: " & entry.path)
      let info = getFileInfo(entry.path, followSymlink = false)
      if info.kind != entry.kind or info.isSpecial or $info.id != entry.identity:
        failSource("entry changed identity/type: " & entry.path)
      let resolved = expandFilename(entry.path)
      if not resolved.startsWith(boundary & DirSep):
        failSource("entry escapes selected source root: " & entry.path)
      case entry.kind
      of pcDir:
        visit(entry.path, boundary)
      of pcFile:
        if not fileExists(entry.path):
          failSource("regular file disappeared: " & entry.path)
        if resolved notin seen:
          seen.incl(resolved)
          files.add(resolved)
      else:
        failSource("unsupported entry: " & entry.path)
    let directoryAfter = getFileInfo(directory, followSymlink = false)
    if directoryAfter.kind != pcDir or directoryAfter.isSpecial or
        directoryAfter.id != directoryBefore.id or members(directory) != before:
      failSource("directory identity/membership changed during traversal: " & directory)

  for root in roots:
    let material = absolutePath(root, project).normalizedPath
    if symlinkExists(material) or not dirExists(material):
      failSource("missing or symlink selected root: " & material)
    let boundary = expandFilename(material)
    if boundary != material or not boundary.startsWith(workspace & DirSep):
      failSource("selected root escapes owning workspace: " & material)
    visit(material, boundary)
  if files.len == 0:
    failSource("empty complete source census")
  files.sort()
  result = files
