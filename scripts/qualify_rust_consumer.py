"""Actual Rust consumer/worker/SQLite integration with independent stored hashes.

Creates one test-owned temporary evidence DB, checks it read-only with Python,
then removes that exact DB. No consumer/canonical/user database is opened.
"""
import argparse
import hashlib
import json
import os
import sqlite3
import subprocess
import tempfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
OFFICIAL='https://www2.camara.leg.br/legin/fed/lei/2018/lei-13709-14-agosto-2018-787077-normaatualizada-pl.pdf'
def digest(value):return hashlib.sha256(json.dumps(value,sort_keys=True,ensure_ascii=False,separators=(',',':')).encode()).hexdigest()
def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--official',action='store_true');args=parser.parse_args()
    temp_root=(ROOT/'tmp').resolve();temp_root.mkdir(exist_ok=True)
    fd,name=tempfile.mkstemp(prefix='r08-rust-consumer-',suffix='.sqlite',dir=temp_root);os.close(fd)
    db=Path(name).resolve()
    if db.parent!=temp_root:raise RuntimeError('Unexpected generated database path')
    db.unlink() # create_new belongs to the Rust adapter
    try:
        source=OFFICIAL if args.official else str(ROOT/'tests/fixtures/two-pages.pdf')
        query='Quem deve reparar danos por tratamento indevido de dados pessoais?' if args.official else 'responsabilidade'
        command=[str(ROOT/'target/debug/examples/rust_consumer.exe'),str(ROOT/'target/debug/retrieval-worker.exe'),source,query,str(db)]
        if args.official:command.append(str(ROOT/'assets/e5-small-int8'))
        process=subprocess.run(command,cwd=ROOT,capture_output=True,text=True,encoding='utf-8',timeout=120,creationflags=subprocess.CREATE_NO_WINDOW)
        if process.returncode:raise RuntimeError('Consumer failed: '+process.stderr[-2000:])
        report=json.loads(process.stdout)
        if not report['worker_stopped'] or report['receipt']!=report['reopened_receipt']:raise RuntimeError('Consumer did not stop/reconcile its receipt')
        connection=sqlite3.connect(db.as_uri()+'?mode=ro',uri=True)
        try:
            application_id=connection.execute('PRAGMA application_id').fetchone()[0]
            tables=connection.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall()
            rows=connection.execute('SELECT namespace,write_id,pack_sha256,payload,receipt FROM retrieval_evidence').fetchall()
        finally:connection.close()
        if application_id!=0x52455631 or tables!=[('retrieval_evidence',)] or len(rows)!=1:raise RuntimeError('Unexpected evidence DB format')
        namespace,write_id,pack_sha,payload,receipt=rows[0];pack=json.loads(payload);stored=json.loads(receipt)
        if digest([pack['schema_version'],pack['payload']])!=pack_sha or pack['sha256']!=pack_sha:raise RuntimeError('Stored pack checksum differs')
        if digest([stored['schema_version'],namespace,write_id,pack_sha,len(payload),stored['verification']])!=stored['receipt_sha256']:raise RuntimeError('Stored receipt checksum differs')
        if stored!=report['receipt'] or stored['verification']!='trusted_worker_export':raise RuntimeError('Receipt binding differs')
        for entry in pack['payload']['entries']:
            record=entry['record'];evidence=entry['evidence']
            if digest(record)!=entry['reference'] or hashlib.sha256(evidence['text'].encode()).hexdigest()!=record['quote_sha256'] or len(evidence['text'])!=record['end']-record['start']:raise RuntimeError('Stored reference/quote differs')
        report['independent_python_readonly_verification']=True
        report['source_provenance']=pack['payload']['documents']
        report['database_bytes']=db.stat().st_size
        report['temporary_database_removed_after_validation']=True
        report['large_llm_calls']=0
        report['official_remote_source']=args.official
        report['external_foundry_admission']=False
        target=ROOT/'verification'/('r08-rust-consumer-official.json' if args.official else 'r08-rust-consumer-synthetic.json')
        target.write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')
        print('Actual Rust worker/SQLite + independent receipt/pack hashes passed;',len(pack['payload']['entries']),'entries')
    finally:
        if db.exists():db.unlink()

if __name__=='__main__':main()
