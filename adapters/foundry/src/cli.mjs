// Foundry v1 requires a Node entrypoint; all retrieval logic stays in Rust.
import {spawn} from 'node:child_process';
import {fileURLToPath} from 'node:url';
const binary=fileURLToPath(new URL('../lib/retrieval-worker.exe',import.meta.url));
const child=spawn(binary,['--foundry-job',...process.argv.slice(2)],{
  env:Object.fromEntries(['PATH','SystemRoot','WINDIR','PATHEXT','TEMP','TMP'].filter(k=>process.env[k]!==undefined).map(k=>[k,process.env[k]])),
  stdio:['ignore','ignore','ignore'],windowsHide:true
});
child.once('error',()=>process.exit(1));
child.once('exit',code=>process.exit(code??1));
process.once('SIGTERM',()=>{child.kill();});
