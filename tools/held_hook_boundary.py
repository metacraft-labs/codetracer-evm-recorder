"""Finite private hook mutation boundary; no owning execution authorized yet.

Every retained descriptor is checked again after the complete final inventory
read. Unknown changes are retained and refused rather than rolled back blindly.
"""
import hashlib
import os
from pathlib import Path
import stat
import subprocess

class HeldHookBoundary:
    def __init__(self, root, common, hooks, git, factory, expected):
        self.root,self.common,self.hooks,self.git,self.factory=root,common,hooks,git,factory
        self.directories={p: self.directory_identity(p) for p in (root,common)}
        self.hooks_identity=self.directory_identity(hooks) if hooks.exists() else None
        self.source=self.source_snapshot()
        self.config=self.config_snapshot()
        self.config_sources=self.config_source_snapshot(self.config)
        self.implementation={p:hashlib.sha256(p.read_bytes()).hexdigest() for p in (Path(__file__),Path(__file__).with_name("install-canonical-hooks.py"))}
        self.entries={}
        try:
            self.entries=self.open_inventory()
            if self.bodies(self.entries)!=expected:raise RuntimeError('hook inventory changed after admission')
            self.guard()
        except BaseException:
            self.close();raise
    @staticmethod
    def directory_identity(p):
        m=p.lstat()
        if not stat.S_ISDIR(m.st_mode):raise RuntimeError('owned directory changed type: '+str(p))
        return m.st_dev,m.st_ino,stat.S_IMODE(m.st_mode)
    def git_read(self,*args):return subprocess.check_output([self.git,*args],cwd=self.root)
    def config_snapshot(self):return self.git_read('config','--null','--show-origin','--list')
    def config_source_snapshot(self, raw):
        fields=raw.split(b'\0')
        if fields[-1]!=b'' or (len(fields)-1)%2:raise RuntimeError('unsupported effective Git config origin framing')
        result={}
        for origin in fields[:-1:2]:
            if not origin.startswith(b'file:'):raise RuntimeError('unqualified nonfile Git configuration authority')
            lexical=Path(os.fsdecode(origin[5:]));path=lexical if lexical.is_absolute() else self.root/lexical
            info=path.lstat()
            result[str(path.absolute())]=(info.st_dev,info.st_ino,stat.S_IMODE(info.st_mode),
                os.readlink(path) if path.is_symlink() else None,hashlib.sha256(path.read_bytes()).hexdigest())
        return result
    def source_snapshot(self):
        index=self.git_read('ls-files','--stage','-z');rows={}
        for entry in index.split(b'\0'):
            if not entry:continue
            metadata,name=entry.split(b'\t',1);p=self.root/os.fsdecode(name);m=p.lstat()
            if metadata.startswith(b'160000 '):raise RuntimeError('unqualified source gitlink in finite hook installer')
            rows[os.fsdecode(name)]=(m.st_dev,m.st_ino,stat.S_IMODE(m.st_mode),
                ('link',os.readlink(p)) if stat.S_ISLNK(m.st_mode) else ('file',hashlib.sha256(p.read_bytes()).hexdigest()))
        return self.git_read('rev-parse','HEAD'),index,rows
    @staticmethod
    def row(fd):
        m=os.fstat(fd)
        if not stat.S_ISREG(m.st_mode) or m.st_nlink!=1:raise RuntimeError('hook descriptor type/link-count changed')
        os.lseek(fd,0,os.SEEK_SET);chunks=[]
        while True:
            chunk=os.read(fd,65536)
            if not chunk:break
            chunks.append(chunk)
        return (m.st_dev,m.st_ino,stat.S_IMODE(m.st_mode),m.st_nlink,b''.join(chunks))
    def open_inventory(self):
        result={}
        try:
            if not self.hooks.exists():return result
            for p in self.hooks.iterdir():
                fd=os.open(p,os.O_RDONLY|os.O_NOFOLLOW)
                try:
                    row=self.row(fd);m=p.lstat()
                    if (m.st_dev,m.st_ino)!=row[:2]:raise RuntimeError('hook pathname changed during held-FD admission')
                    result[p.name]=(fd,row)
                except BaseException:os.close(fd);raise
            return result
        except BaseException:
            for fd,_ in result.values():os.close(fd)
            raise
    @staticmethod
    def bodies(entries):return {n:(r[4],r[2]) for n,(_,r) in entries.items()}
    def guard_authority(self):
        self.factory.guard()
        if any(hashlib.sha256(p.read_bytes()).hexdigest()!=v for p,v in self.implementation.items()):raise RuntimeError("hook boundary/installer implementation changed")
        if any(self.directory_identity(p)!=v for p,v in self.directories.items()):raise RuntimeError('root/common directory identity changed')
        actual=self.directory_identity(self.hooks) if self.hooks.exists() else None
        if actual!=self.hooks_identity:raise RuntimeError('hook directory identity changed')
        if self.source_snapshot()!=self.source:raise RuntimeError('owning HEAD/index/tracked source changed')
        if self.config_snapshot()!=self.config or self.config_source_snapshot(self.config)!=self.config_sources:raise RuntimeError('owning Git configuration/source provenance changed')
    def guard(self, ignored_temporary=None):
        self.guard_authority()
        names={p.name for p in self.hooks.iterdir()} if self.hooks.exists() else set()
        if ignored_temporary is not None:names.discard(ignored_temporary.name)
        if names!=set(self.entries):raise RuntimeError('hook entry inventory changed before mutation')
        # Recompute full held bytes/mode/device/inode/linkcount after inventory.
        for name,(fd,old) in self.entries.items():
            m=(self.hooks/name).lstat()
            if (m.st_dev,m.st_ino)!=old[:2] or self.row(fd)!=old:raise RuntimeError('held hook body/mode/identity changed before mutation: '+name)
    def accept_created_directory(self):
        if self.hooks_identity is not None:raise RuntimeError('hook directory was already present')
        self.hooks_identity=self.directory_identity(self.hooks)
        if list(self.hooks.iterdir()):raise RuntimeError('new hook directory contains unexpected entries')
        self.guard()
    def accept_config_repair(self, old_local):
        current=self.git_read('config','--local','--null','--list')
        def without_hook_path(raw):return sorted(row for row in raw.split(b'\0') if row and not row.startswith(b'core.hookspath\n'))
        if without_hook_path(current)!=without_hook_path(old_local) or self.git_read('config','--local','--get','core.hooksPath').strip()!=os.fsencode(self.hooks):raise RuntimeError('configuration repair changed more than exact owning hook path')
        actual=self.config_snapshot()
        allowed=self.common/'config'
        if allowed.is_symlink() or not allowed.is_file():raise RuntimeError('owning local configuration provenance changed')
        def project(raw):
            fields=raw.split(b'\0')
            if fields[-1]!=b'' or (len(fields)-1)%2:raise RuntimeError('unsupported effective configuration framing')
            rows=[];owned=[]
            for origin,value in zip(fields[:-1:2],fields[1:-1:2]):
                if origin.startswith(b'file:'):
                    path=Path(os.fsdecode(origin[5:]));path=path if path.is_absolute() else self.root/path
                else:raise RuntimeError('unqualified effective config authority')
                if path.absolute()==allowed and value.startswith(b'core.hookspath\n'):owned.append(value)
                else:rows.append((origin,value))
            return rows,owned
        before,old_owned=project(self.config);after,new_owned=project(actual)
        if before!=after or new_owned!=[b'core.hookspath\n'+os.fsencode(self.hooks)]:raise RuntimeError('effective configuration changed outside exact owned local hook path')
        sources=self.config_source_snapshot(actual)
        if set(sources)!=set(self.config_sources) or any(v!=sources[k] for k,v in self.config_sources.items() if k!=str(allowed)):raise RuntimeError('unrelated effective configuration source changed during local repair')
        self.config=actual;self.config_sources=sources;self.guard()
    def accept(self, expected, changed):
        self.guard_authority()
        fresh=self.open_inventory()
        try:
            if self.bodies(fresh)!=expected:raise RuntimeError('mutation did not produce exact expected complete hook inventory')
            for name,(fd,old) in self.entries.items():
                if name not in changed and (name not in fresh or fresh[name][1]!=old or self.row(fd)!=old):raise RuntimeError('mutation changed unowned held hook: '+name)
        except BaseException:
            for fd,_ in fresh.values():os.close(fd)
            raise
        self.close();self.entries=fresh;self.guard()
    def close(self):
        for fd,_ in self.entries.values():os.close(fd)
        self.entries={}
