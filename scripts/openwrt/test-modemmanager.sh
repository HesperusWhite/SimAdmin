#!/bin/sh
# Filesystem fixtures + fake daemon + PATH bus mocks. Never contact host MM,
# serial devices, netifd or the system bus.
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
export SIMADMIN_TEST_REAL_SLEEP="$(command -v sleep)"
export SIMADMIN_TEST_REAL_DATE="$(command -v date)"
node - "$script_dir" <<'EOF_NODE'
const fs = require('fs');
const os = require('os');
const path = require('path');
const assert = require('assert/strict');
const { spawn, spawnSync } = require('child_process');
const scripts = process.argv[2];
const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'simadmin-mm-test-'));
const sys = path.join(fixture, 'sys');
const dev = path.join(fixture, 'dev');
const bin = path.join(fixture, 'bin');
const trace = path.join(fixture, 'trace');
const pidfile = path.join(fixture, 'mm.pid');
const activation = path.join(fixture, 'activation.service');
const nativePidfile = path.join(fixture, 'native.pid');
const enabled = path.join(fixture, 'enabled');
const clock = path.join(fixture, 'clock');
const proc = path.join(fixture, 'proc');
fs.mkdirSync(`${proc}/123/fd`, {recursive:true});
fs.writeFileSync(activation, 'Exec=/bin/false\n');
for (const dir of [dev, bin, `${sys}/class/net`, `${sys}/class/tty`]) fs.mkdirSync(dir, {recursive:true});
const physical = `${sys}/devices/usb3/3-1`;
const otherPhysical = `${sys}/devices/usb3/3-2`;
function deviceTree(root) {
  fs.mkdirSync(`${root}/3-1:1.0/net/usb0`, {recursive:true});
  fs.mkdirSync(`${root}/3-1:1.4/ttyUSB2/tty/ttyUSB2`, {recursive:true});
  fs.writeFileSync(`${root}/idVendor`, '2c7c\n');
  fs.writeFileSync(`${root}/idProduct`, '0900\n');
  fs.writeFileSync(`${root}/3-1:1.0/bInterfaceNumber`, '00\n');
  fs.writeFileSync(`${root}/3-1:1.4/bInterfaceNumber`, '04\n');
}
deviceTree(physical);
deviceTree(otherPhysical);
const netLink = `${sys}/class/net/usb0`;
const ttyLink = `${sys}/class/tty/ttyUSB2`;
const ttyTarget = `${physical}/3-1:1.4/ttyUSB2/tty/ttyUSB2`;
fs.symlinkSync(`${physical}/3-1:1.0/net/usb0`, netLink);
fs.symlinkSync(ttyTarget, ttyLink);
// Metadata only: fake MM never opens this character-device fixture.
fs.symlinkSync('/dev/null', `${dev}/ttyUSB2`);
function executable(name, content) {
  const file = `${bin}/${name}`;
  fs.writeFileSync(file, content, {mode:0o755});
  return file;
}
executable('logger', '#!/bin/sh\nprintf "LOG %s\\n" "$*" >> "$MOCK_TRACE"\n');
executable('sleep', '#!/bin/sh\n"$SIMADMIN_TEST_REAL_SLEEP" 0.01\n');
executable('date', "#!/bin/sh\nif [ \"$MOCK_MODE\" = discovery-timeout ]; then\n  mm_test_clock=0\n  [ ! -f \"$MOCK_CLOCK\" ] || mm_test_clock=$(cat \"$MOCK_CLOCK\")\n  printf '%s\\n' \"$mm_test_clock\"\n  printf '%s\\n' \"$((mm_test_clock + 20))\" > \"$MOCK_CLOCK\"\nelse\n  \"$SIMADMIN_TEST_REAL_DATE\" \"$@\"\nfi\n");
executable('jsonfilter', "#!/usr/bin/env node\nconst fs = require('fs');\nlet value;\ntry {value = JSON.parse(fs.readFileSync(0, 'utf8'));} catch {process.exit(126);}\nconst [mode, pattern] = process.argv.slice(2);\nconst expr = pattern.replace(/\\[\"([^\"\\]]+)\"\\]/g, '.$1').replace(/\\[\\*\\]/g, '.*');\nif (!['-e','-t'].includes(mode) || !/^@(?:\\.(?:\\*|[a-zA-Z_][a-zA-Z_0-9-]*))*$/.test(expr)) process.exit(99);\nlet values = [value];\nfor (const key of expr.slice(1).split('.').filter(Boolean)) {\n  values = values.flatMap(v => v && typeof v === 'object' ? key === '*' ? Object.values(v) : Object.hasOwn(v,key) ? [v[key]] : [] : []);\n}\nif (!values.length) process.exit(1);\nconst type = v => v === null ? 'null' : Array.isArray(v) ? 'array' : typeof v;\nconst result = mode === '-t' ? values.map(type).join('\\\\ ') : values.map(v => typeof v === 'string' ? v : JSON.stringify(v)).join('\\n');\nprocess.stdout.write(result + '\\n');\n");
executable('dbus-send', `#!/bin/sh
case "$*" in
  *GetConnectionUnixProcessID*)
    [ -f "$MOCK_PIDFILE" ] || exit 1
    mm_test_pid=$(cat "$MOCK_PIDFILE")
    [ "$MOCK_MODE" != wrong-owner ] || mm_test_pid=1
    printf 'method return\n   uint32 %s\n' "$mm_test_pid"
    ;;
  *SetLogging*)
    printf 'LOGGING INFO\n' >> "$MOCK_TRACE"
    [ "$MOCK_MODE" != logging-fail ] || exit 1
    printf 'method return\n'
    ;;
  *) exit 99 ;;
esac
`);
executable('mmcli', "#!/bin/sh\ncase \"$1\" in --timeout=*) shift ;; *) exit 99 ;; esac\ncase \"$*\" in\n  --report-kernel-event=action=add,subsystem=net,name=usb0|--report-kernel-event=action=add,subsystem=tty,name=ttyUSB2)\n    printf 'EVENT %s\\n' \"$*\" >> \"$MOCK_TRACE\"\n    if [ \"$MOCK_MODE\" = report-fail ] && [ \"$*\" = --report-kernel-event=action=add,subsystem=tty,name=ttyUSB2 ]; then exit 1; fi\n    ;;\n  '-L -J')\n    if [ \"$MOCK_MODE\" = discovery-timeout ]; then echo '{\"modem-list\":[]}'; else echo '{\"modem-list\":[\"/org/freedesktop/ModemManager1/Modem/0\"]}'; fi\n    ;;\n  '-m /org/freedesktop/ModemManager1/Modem/0 -J')\n    mm_test_state=disabled\n    [ ! -f \"$MOCK_ENABLED\" ] || mm_test_state=enabled\n    printf '{\"modem\":{\"generic\":{\"model\":\"RG200U-CN\",\"ports\":[\"ttyUSB2 (at)\",\"usb0 (net)\"],\"primary-port\":\"ttyUSB2\",\"device\":\"%s/devices/usb3/3-1\",\"state\":\"%s\"}}}\\n' \"$MOCK_SYSROOT\" \"$mm_test_state\"\n    ;;\n  '-m /org/freedesktop/ModemManager1/Modem/0 --enable')\n    printf 'ENABLE /org/freedesktop/ModemManager1/Modem/0\\n' >> \"$MOCK_TRACE\"\n    [ \"$MOCK_MODE\" != enable-fail ] || exit 1\n    : > \"$MOCK_ENABLED\"\n    ;;\n  *) exit 99 ;;\nesac\n");
const fakeMM = executable('fake-mm', `#!/bin/sh
trap 'printf "MMSTOP\n" >> "$MOCK_TRACE"; exit 0' TERM INT
printf 'MM %s\n' "$*" >> "$MOCK_TRACE"
printf '%s\n' "$$" > "$MOCK_PIDFILE"
[ "$MOCK_MODE" != daemon-lost ] || exit 17
while :; do "$SIMADMIN_TEST_REAL_SLEEP" 0.02; done
`);
const library = `${scripts}/modemmanager-ports.sh`;
const harness = `${fixture}/ports-fixture.sh`;
fs.writeFileSync(harness, `. "$MOCK_REAL_LIBRARY"\nsimadmin_mm_validate_ports() { simadmin_mm_validate_runtime_at "$MOCK_ACTIVATION" "$MOCK_NATIVE_PIDFILE" && simadmin_mm_validate_ports_at "$MOCK_SYSROOT" "$MOCK_DEVROOT"; }\nsimadmin_mm_require_serial_free() { simadmin_mm_require_serial_free_at "$MOCK_PROCROOT"; }\n`);
const env = {...process.env, PATH:`${bin}:${process.env.PATH}`, MOCK_TRACE:trace, MOCK_PIDFILE:pidfile,
  MOCK_REAL_LIBRARY:library, MOCK_SYSROOT:sys, MOCK_DEVROOT:dev, MOCK_ACTIVATION:activation, MOCK_NATIVE_PIDFILE:nativePidfile, MOCK_ENABLED:enabled, MOCK_CLOCK:clock, MOCK_PROCROOT:proc};
function validate(expected) {
  const result = spawnSync('sh', ['-c', '. "$1"; simadmin_mm_validate_ports_at "$2" "$3"', 'fixture', library, sys, dev], {env});
  assert.equal(result.status === 0, expected, result.stderr.toString());
}
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const readTrace = () => fs.existsSync(trace) ? fs.readFileSync(trace, 'utf8') : '';
async function runWrapper(mode, success = false) {
  fs.writeFileSync(trace, '');
  fs.rmSync(pidfile, {force:true});
  fs.rmSync(enabled, {force:true});
  fs.rmSync(clock, {force:true});
  const child = spawn('sh', [`${scripts}/modemmanager-wrapper.sh`, fakeMM,
    `${scripts}/modemmanager-set-logging.sh`, harness], {env:{...env, MOCK_MODE:mode}, stdio:['ignore','pipe','pipe']});
  let diagnostics = '';
  child.stderr.on('data', chunk => diagnostics += chunk);
  const exited = new Promise(resolve => child.once('exit', (code, signal) => resolve({code, signal})));
  let finished = false;
  exited.then(() => finished = true);
  try {
    const deadline = Date.now() + 4000;
    if (success) {
      while (!finished && !readTrace().includes('initialization ready') && Date.now() < deadline) await pause(10);
      assert(!finished, `wrapper exited before discovery: ${diagnostics}`);
      const lines = readTrace().trim().split('\n');
      assert(lines[0].startsWith('MM '));
      for (const option of ['--debug', '--no-auto-scan', '--test-no-qrtr', '--filter-policy=ALLOWLIST-ONLY']) assert(lines[0].includes(option));
      const events = lines.filter(v => v.startsWith('EVENT '));
      assert.deepEqual(events, [
        'EVENT --report-kernel-event=action=add,subsystem=net,name=usb0',
        'EVENT --report-kernel-event=action=add,subsystem=tty,name=ttyUSB2',
      ]);
      assert(lines.indexOf('LOGGING INFO') < lines.findIndex(v => v.startsWith('EVENT ')));
      assert.equal(lines.filter(v => v.startsWith('ENABLE ')).length, 1);
      assert(lines.findIndex(v => v.startsWith('ENABLE ')) > lines.findLastIndex(v => v.startsWith('EVENT ')));
      child.kill('SIGTERM');
    }
    const result = await Promise.race([exited, pause(4500).then(() => {throw Error(`wrapper timeout (${mode}): ${diagnostics}`);})]);
    if (success) {
      assert.equal(result.code, 143);
      assert(readTrace().includes('MMSTOP'));
    } else {
      assert.notEqual(result.code, 0, `accepted failure mode ${mode}`);
      if (['daemon-lost','wrong-owner','logging-fail','missing-serial','serial-busy'].includes(mode)) assert(!readTrace().includes('EVENT '));
      if (['missing-serial','serial-busy'].includes(mode)) assert(!readTrace().includes('MM '));
      if (!['daemon-lost','missing-serial','serial-busy'].includes(mode)) assert(readTrace().includes('MMSTOP'), `child not cleaned (${mode})`);
    }
  } finally {
    if (!finished) { child.kill('SIGTERM'); await Promise.race([exited, pause(1500)]); }
  }
}
(async () => {
  validate(true);
  const runtimeCheck = expected => {
    const result = spawnSync('sh', ['-c', '. "$1"; simadmin_mm_validate_runtime_at "$2" "$3"', 'fixture', library, activation, nativePidfile], {env});
    assert.equal(result.status === 0, expected);
  };
  runtimeCheck(true);
  fs.writeFileSync(nativePidfile, '123\n'); runtimeCheck(false);
  fs.rmSync(nativePidfile);
  fs.writeFileSync(activation, 'Exec=/usr/sbin/ModemManager\n'); runtimeCheck(false);
  fs.writeFileSync(activation, 'Exec=/bin/false\n'); runtimeCheck(true);
  fs.symlinkSync('/dev/ttyUSB2', proc + '/123/fd/3');
  await runWrapper('serial-busy');
  fs.unlinkSync(proc + '/123/fd/3');
  fs.unlinkSync(dev + '/ttyUSB2'); validate(false);
  await runWrapper('missing-serial');
  fs.symlinkSync('/dev/null', `${dev}/ttyUSB2`);
  fs.unlinkSync(ttyLink); fs.symlinkSync(`${otherPhysical}/3-1:1.4/ttyUSB2/tty/ttyUSB2`, ttyLink); validate(false);
  fs.unlinkSync(ttyLink); fs.symlinkSync(ttyTarget, ttyLink);
  fs.writeFileSync(`${physical}/idProduct`, '0800\n'); validate(false);
  fs.writeFileSync(`${physical}/idProduct`, '0900\n');
  fs.writeFileSync(`${physical}/3-1:1.4/bInterfaceNumber`, '02\n'); validate(false);
  fs.writeFileSync(`${physical}/3-1:1.4/bInterfaceNumber`, '04\n');
  validate(true);
  const baseModem = {modem:{generic:{model:'RG200U-CN', ports:['ttyUSB2 (at)','usb0 (net)'], 'primary-port':'ttyUSB2', device:physical, state:'disabled'}}};
  const matches = (value, expected) => {
    const result = spawnSync('sh', ['-c', '. "$1"; simadmin_mm_validate_ports_at "$2" "$3" && simadmin_mm_matches_target "$MOCK_JSON"', 'fixture', library, sys, dev], {env:{...env, MOCK_JSON:JSON.stringify(value)}});
    assert.equal(result.status === 0, expected);
  };
  matches(baseModem, true);
  for (const update of [{model:'other'}, {device:otherPhysical}, {'primary-port':'ttyUSB3'},
      {ports:['ttyUSB2 (at)','usb1 (net)']}, {ports:['ttyUSB2 (at)','usb0 (net)','ttyUSB3 (at)']},
      {ports:['ttyUSB2 (at)','ttyUSB2 (at)','usb0 (net)']}]) {
    const invalid = structuredClone(baseModem);
    Object.assign(invalid.modem.generic, update);
    matches(invalid, false);
  }
  await runWrapper('success', true);
  for (const mode of ['daemon-lost', 'wrong-owner', 'logging-fail', 'report-fail', 'enable-fail', 'discovery-timeout']) await runWrapper(mode);
  console.log('Restricted MM tests passed (identity, exact ports, Enable/readiness, discovery deadline, owner/failure cleanup).');
})().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => fs.rmSync(fixture, {recursive:true, force:true}));
EOF_NODE
