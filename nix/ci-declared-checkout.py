#!/usr/bin/env python3
"""Create a genuine exact-event checkout with declared Git; no shell activation."""
import hashlib,json,os,pathlib,stat,subprocess,sys,tempfile

def require(ok,message):
    if not ok: raise RuntimeError(message)

def main():
    git,templates,source,destination,event,workspace=sys.argv[1:]
    git=pathlib.Path(git);templates=pathlib.Path(templates)
    require(git.is_absolute() and str(git).startswith("/nix/store/") and git.is_file() and os.access(git,os.X_OK),"Foreign declared Git")
    source=pathlib.Path(source).resolve(strict=True)
    workspace=pathlib.Path(workspace).resolve(strict=True)
    raw=pathlib.Path(destination)
    require(raw.is_absolute() and not raw.exists() and not raw.is_symlink(),"Existing destination")
    parent=raw.parent.resolve(strict=True)
    require(parent==workspace and raw.name not in ("", ".", "..") and source!=raw,"Escaped or foreign destination")
    # GitHub setup-nix exports scoped credential GIT_CONFIG_* authority.
    # Local creator metadata/object transfer needs no network credential;
    # isolate ALL Git child authority while leaving parent auth unchanged.
    env={key:value for key,value in os.environ.items() if not key.startswith("GIT_")}
    env.update(GIT_CONFIG_NOSYSTEM="1",GIT_CONFIG_GLOBAL="/dev/null")
    script_identity=hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest()
    tool_identity=hashlib.sha256(git.read_bytes()).hexdigest()
    def object_identity(path):
        st=path.lstat()
        return [st.st_dev,st.st_ino,stat.S_IMODE(st.st_mode)]
    source_root_identity=object_identity(source)
    require(not any(x.is_symlink() for x in templates.rglob("*")),"Template symlink refused")
    def run(args,cwd):
        return subprocess.check_output([str(git),"--no-optional-locks","-c","core.fsmonitor=false",*args],cwd=cwd,env=env)
    def inventory(root):
        result={}
        for item in sorted(root.rglob("*")):
            rel=item.relative_to(root)
            if rel.parts[0]==".git": continue
            st=item.lstat()
            if stat.S_ISLNK(st.st_mode): value=["link",stat.S_IMODE(st.st_mode),os.readlink(item)]
            elif stat.S_ISREG(st.st_mode): value=["file",stat.S_IMODE(st.st_mode),hashlib.sha256(item.read_bytes()).hexdigest()]
            elif stat.S_ISDIR(st.st_mode): value=["directory",stat.S_IMODE(st.st_mode)]
            else: raise RuntimeError("Unknown source entry")
            result[str(rel)]=value
        return result
    template_before=inventory(templates)
    template_root_before=object_identity(templates)
    before_source=inventory(source)
    common=pathlib.Path(run(["rev-parse","--path-format=absolute","--git-common-dir"],source).decode().strip())
    gitdir=pathlib.Path(run(["rev-parse","--absolute-git-dir"],source).decode().strip())
    config_before=(common/"config").read_bytes()
    authority_before={"root":source_root_identity,"common":object_identity(common),"gitdir":object_identity(gitdir),"index":hashlib.sha256((gitdir/"index").read_bytes()).hexdigest()}
    local_config=run(["config","--local","--no-includes","--name-only","--list"],source).decode().splitlines()
    require(not any(k.startswith(("filter.","include.","includeif.","alias.")) or k in ("core.hookspath","core.fsmonitor","core.attributesfile","core.sshcommand") for k in local_config),"Source execution/filter authority refused")
    actual=run(["rev-parse","HEAD"],source).decode().strip()
    require(actual==event and len(event)==40,"Event HEAD mismatch")
    require(not run(["status","--porcelain=v1","--untracked-files=no"],source),"Dirty source")
    require(not run(["ls-tree","-r",event,"--",".gitmodules"],source),"Submodules require explicit support")
    rows=run(["ls-tree","-rz",event],source).split(b"\0")
    for row in rows:
        if not row: continue
        meta,name=row.split(b"\t",1)
        require(not meta.startswith(b"160000 "),"Submodule tree refused")
        if meta.startswith(b"100"):
            body=run(["cat-file","blob",meta.split()[2].decode()],source)
            require(not body.startswith(b"version https://git-lfs.github.com/spec/v1\n"),"LFS requires explicit support")
    require(templates.is_absolute() and templates.is_dir(),"Missing declared templates")
    os.mkdir(raw,0o700)
    owned=os.stat(raw,follow_symlinks=False)
    def directory_guard():
        now=os.stat(raw,follow_symlinks=False)
        require((now.st_dev,now.st_ino)==(owned.st_dev,owned.st_ino) and stat.S_ISDIR(now.st_mode),"Destination directory replaced")
    # Owned partial checkout is intentionally retained on any failure.
    execpath=pathlib.Path(run(["--exec-path"],source).decode().strip())
    upload=execpath/"git-upload-pack"
    require(upload.is_file() and os.access(upload,os.X_OK) and str(upload).startswith("/nix/store/"),"Foreign upload-pack")
    subprocess.run([str(git),"clone","--no-local","--no-checkout","--upload-pack="+str(upload),"--template="+str(templates),str(source),str(raw)],env=env,check=True)
    directory_guard()
    subprocess.run([str(git),"checkout","--detach",event],cwd=raw,env=env,check=True)
    directory_guard()
    require(run(["rev-parse","HEAD"],raw).decode().strip()==event,"Checkout HEAD mismatch")
    require(run(["ls-tree","-rz",event],raw)==b"\0".join(rows),"Tree mismatch")
    require(not run(["status","--porcelain=v1","--untracked-files=no"],raw),"Checkout dirty")
    require(run(["rev-list",event],raw)==run(["rev-list",event],source),"Event ancestry mismatch")
    require(run(["rev-parse","--is-shallow-repository"],raw)==run(["rev-parse","--is-shallow-repository"],source),"Shallow ancestry boundary mismatch")
    for row in rows:
        if not row: continue
        meta,name=row.split(b"\t",1)
        mode,kind,oid=meta.split()
        entry=raw/os.fsdecode(name)
        require(entry.parent.resolve().is_relative_to(raw),"Escaped worktree entry")
        body=run(["cat-file","blob",oid.decode()],source)
        if mode==b"120000":
            require(entry.is_symlink() and os.fsencode(os.readlink(entry))==body,"Worktree link mismatch")
        else:
            require(entry.is_file() and not entry.is_symlink() and entry.read_bytes()==body,"Worktree bytes mismatch")
            require(stat.S_IMODE(entry.stat().st_mode)==(0o755 if mode==b"100755" else 0o644),"Worktree mode mismatch")
    require(inventory(source)==before_source,"Bootstrap source changed")
    require(run(["rev-parse","HEAD"],source).decode().strip()==event,"Bootstrap HEAD changed")
    authority_after={"root":object_identity(source),"common":object_identity(common),"gitdir":object_identity(gitdir),"index":hashlib.sha256((gitdir/"index").read_bytes()).hexdigest()}
    require(authority_before==authority_after and (common/"config").read_bytes()==config_before,"Bootstrap Git authority changed")
    require(hashlib.sha256(git.read_bytes()).hexdigest()==tool_identity and hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest()==script_identity,"Principal changed")
    template_after=inventory(templates)
    require(template_before==template_after and object_identity(templates)==template_root_before,"Templates changed")
    hooks=raw/".git/hooks"
    expected={x.name:x.read_bytes() for x in (templates/"hooks").iterdir() if x.is_file()}
    require(len(expected)==14 and set(expected)=={x.name for x in hooks.iterdir()},"Complete14 hooks mismatch")
    require(all((hooks/name).read_bytes()==body and stat.S_IMODE((hooks/name).stat().st_mode)==0o755 for name,body in expected.items()),"Initialized hook mismatch")
    policy=subprocess.run([str(git),"config","--get","core.sharedRepository"],cwd=raw,env=env,capture_output=True)
    require(policy.returncode==1 and not policy.stdout,"Unexpected shared policy")
    print(json.dumps({"event":event,"git":str(git),"git_sha256":hashlib.sha256(git.read_bytes()).hexdigest(),"source":str(source),"destination":str(raw),"ancestry_equal":True,"tree_equal":True,"templates":str(templates),"template_inventory":template_after,"initialized_hook_inventory":{name:{"sha256":hashlib.sha256((hooks/name).read_bytes()).hexdigest(),"mode":stat.S_IMODE((hooks/name).stat().st_mode)} for name in expected}}))

if __name__=="__main__":
    original_mask=os.umask(0o022)
    try:
        main()
    finally:
        os.umask(original_mask)
