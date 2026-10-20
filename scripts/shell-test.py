#!/usr/bin/env python3
"""Test the shell and full-screen app lifecycle on an isolated FAT disk."""
import contextlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import tempfile
import time
ROOT=Path(__file__).resolve().parents[1]
BUILD=ROOT/'build'
class Monitor:
    def __init__(self,path,proc):
        deadline=time.monotonic()+15
        while True:
            self.sock=socket.socket(socket.AF_UNIX)
            try:self.sock.connect(str(path));break
            except (FileNotFoundError,ConnectionRefusedError):
                self.sock.close()
                if proc.poll() is not None:raise RuntimeError('QEMU exited; see build/shell-qemu.log')
                if time.monotonic()>deadline:raise TimeoutError('QMP socket')
                time.sleep(.1)
        self.stream=self.sock.makefile('rwb',buffering=0)
        self.stream.readline();self.call('qmp_capabilities')
    def call(self,command,**arguments):
        self.stream.write((json.dumps({'execute':command,'arguments':arguments})+'\n').encode())
        while True:
            value=json.loads(self.stream.readline())
            if 'error' in value:raise RuntimeError(value)
            if 'return' in value:return value['return']
    def hmp(self,text):return self.call('human-monitor-command',**{'command-line':text})
    def key(self,name):self.hmp('sendkey '+name+' 30');time.sleep(.065)
    def type(self,text):
        names={' ':'spc','/':'slash','.':'dot','-':'minus','_':'shift-minus','"':'shift-apostrophe',"'":'apostrophe',';':'semicolon','\\':'backslash',':':'shift-semicolon','(':'shift-9',')':'shift-0','>':'shift-dot','<':'shift-comma'}
        for char in text:
            self.key(names.get(char,('shift-'+char.lower())if char.isupper()else char))
    def line(self,text):self.type(text);self.key('ret')
    def close(self):self.stream.close();self.sock.close()
def main():
    BUILD.mkdir(exist_ok=True)
    log=BUILD/'shell-serial.log'
    with tempfile.TemporaryDirectory(prefix='nodeos-shell-')as temp:
        temp=Path(temp);disk=temp/'disk.img';shutil.copyfile(BUILD/'disk.img',disk)
        script=temp/'SHELL.NSH'
        script.write_text('# quotes, copy protection, scripts and path normalization\nwrite /QUOTE.TXT "alpha beta\\nsecond line\\n"\ncp /QUOTE.TXT /COPY.TXT\nappend /COPY.TXT "end\\n"\ncp /QUOTE.TXT /COPY.TXT\ncp /COPY.TXT /copy.txt\ngrep "second line" /COPY.TXT\ncd /docs; cd ..\ncp /TESTS/BIG.BIN /BIG.BIN\ngrep needle /TESTS/WIDE.TXT\nwrite /DONE.TXT "script complete"\n')
        subprocess.run(['mcopy','-o','-i',str(disk),str(script),'::/TESTS/SHELL.NSH'],check=True)
        long=temp/'LONG.TXT';long.write_text(''.join(f'line {i:04d}\n' for i in range(300)))
        subprocess.run(['mcopy','-o','-i',str(disk),str(long),'::/TESTS/LONG.TXT'],check=True)
        binary=temp/'BIG.BIN';binary.write_bytes(bytes(range(256))*400)
        wide=temp/'WIDE.TXT';wide.write_text('a'*16000+' needle\n')
        for fixture in (binary,wide):subprocess.run(['mcopy','-o','-i',str(disk),str(fixture),'::/TESTS/'+fixture.name],check=True)
        with (BUILD/'shell-qemu.log').open('wb')as errors:
            proc=subprocess.Popen(['qemu-system-x86_64','-machine','q35','-cpu','qemu64','-m','512M','-boot','d','-no-reboot','-display','none','-cdrom',str(BUILD/'nodeos.iso'),'-drive',f'file={disk},if=virtio,format=raw','-audiodev','none,id=snd0','-device','AC97,audiodev=snd0','-serial',f'file:{log}','-qmp',f'unix:{temp}/qmp.sock,server=on,wait=off'],stdout=errors,stderr=subprocess.STDOUT)
            try:
                with contextlib.closing(Monitor(temp/'qmp.sock',proc))as mon:
                    def wait(marker,timeout=30,after=0):
                        end=time.monotonic()+timeout
                        while time.monotonic()<end:
                            text=log.read_text(errors='replace')if log.exists()else''
                            assert 'kernel panic' not in text and ' panicked at ' not in text,text[-2000:]
                            if marker in text[after:]:return text
                            time.sleep(.1)
                        raise AssertionError(f'{marker} not observed\n{text[-2400:]}')
                    wait('shell: ready (fullscreen)')
                    mon.hmp(f'screendump {BUILD}/shell-boot.ppm')
                    assert 'desktop' not in log.read_text().lower()
                    mon.line('help')
                    time.sleep(.3);mon.hmp(f'screendump {BUILD}/shell-help.ppm')
                    for size in ('large','small','normal'):
                        marker=len(log.read_text(errors='replace'))
                        mon.line('font '+size)
                        wait('shell: command complete: font',after=marker)
                        mon.hmp(f'screendump {BUILD}/shell-font-{size}.ppm')
                    mon.line('clear')
                    mon.line('source /tests/shell.nsh')
                    wait('shell: source completed (11 commands)')
                    wait('shell: grep matched 1 line(s)')
                    mon.type('write /EDIT.TXT wrong');mon.key('ctrl-w');mon.line('right')
                    mon.type('write /KEY.TXT ab');mon.key('left');mon.key('delete');mon.line('z')
                    mon.line('write /BAD.TXT forbidden; echo "')
                    wait('shell: syntax rejected')
                    mon.type('write /WORD.TXT ab');mon.key('ctrl-left');mon.key('ctrl-d');mon.key('ctrl-right');mon.line('c')

                    mon.line('more /tests/long.txt');wait('shell: pager waiting')
                    mon.key('q')
                    mon.line('sysinfo');wait('shell: completed /bin/sysinfo.nxf')
                    mon.line('tree /tests');wait('shell: completed /bin/tree.nxf')
                    mon.line('clear; ls /bin')
                    time.sleep(.3);mon.hmp(f'screendump {BUILD}/shell-listing.ppm')
                    mon.line('help cp')
                    mon.line('cp')
                    mon.line('unknowncommand')
                    mon.type('c');mon.key('tab');mon.key('tab');time.sleep(.2)
                    mon.hmp(f'screendump {BUILD}/shell-completions.ppm')
                    mon.key('ctrl-c')
                    mon.type('cd /doc');mon.key('tab');mon.key('ret')
                    mon.line('pwd; cd /')
                    mon.line('help keys; history')
                    time.sleep(.2)
                    mon.hmp(f'screendump {BUILD}/shell-workflows.ppm')
                    mon.key('shift-pgup');time.sleep(.2)
                    mon.hmp(f'screendump {BUILD}/shell-scrollback.ppm')
                    mon.key('shift-pgdn')
                    start=time.monotonic();mon.line('doom')
                    text=wait('doom: IWAD mapped',45)
                    print(f'shell-test: Doom WAD read {re.search(r"IWAD mapped, .* in (\d+) ms",text).group(1)} ms',flush=True)
                    wait('ST_Init: Init status bar.')
                    time.sleep(.3);mon.hmp(f'screendump {BUILD}/shell-doom.ppm')
                    mon.key('scroll_lock-q');wait('shell: completed /bin/doom.nxf')
                    marker=len(log.read_text(errors='replace'))
                    mon.line('write /AFTER.TXT returned')
                    wait('shell: command complete: write',after=marker)
                    time.sleep(.1);mon.hmp(f'screendump {BUILD}/shell-restored.ppm')
            finally:
                proc.terminate();proc.wait(timeout=15)
        def read(path):return subprocess.check_output(['mtype','-i',str(disk),'::/'+path]).decode()
        assert read('QUOTE.TXT')=='alpha beta\nsecond line\n'
        assert read('COPY.TXT')=='alpha beta\nsecond line\nend\n'
        assert subprocess.check_output(['mtype','-i',str(disk),'::/BIG.BIN'])==binary.read_bytes()
        assert read('DONE.TXT')=='script complete'
        assert read('EDIT.TXT')=='right'
        assert read('KEY.TXT')=='az'
        assert read('WORD.TXT')=='bc'
        assert read('AFTER.TXT')=='returned'
        assert subprocess.run(['mdir','-i',str(disk),'::/BAD.TXT'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode!=0
        assert (BUILD/'shell-workflows.ppm').read_bytes()!=(BUILD/'shell-scrollback.ppm').read_bytes()
        print('shell-test: PASS (quotes, scripts, copy protection, grep, editing, pager, scrollback, foreground apps, Doom return)',flush=True)
if __name__=='__main__':main()
