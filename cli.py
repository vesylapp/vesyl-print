"""vesyl-print CLI: claim, enroll, status, unpair, agent, print-test, update."""

from __future__ import annotations

import argparse
import json
import logging
import sys
from pathlib import Path

import agent as agent_mod
import auth
import jobs
import printers
import statusio
import sysinfo
import update as update_mod
from cloud import CloudClient, CloudError
from config import AGENT_VERSION, default_platform, load_config, write_default_config


def _die(msg: str, code: int = 1) -> None:
    print(msg, file=sys.stderr)
    raise SystemExit(code)


def cmd_claim(args: argparse.Namespace) -> int:
    cfg = load_config()
    cfg.ensure_dirs()
    write_default_config(cfg.config_path)

    code = args.code.strip().replace("-", "").replace(" ", "")
    if len(code) < 6:
        _die("claim code looks too short")

    client = CloudClient(cfg.api_base_url)
    try:
        data = client.claim(
            code,
            hostname=sysinfo.hostname(),
            agent_version=AGENT_VERSION,
            platform=default_platform(),
            name=args.name,
        )
    except CloudError as e:
        _die(f"claim failed: {e.message}" + (f" ({e.code})" if e.code else ""))

    token = data.get("device_token")
    if not token:
        _die("claim response missing device_token")

    creds = auth.credentials_from_pair_response(data)
    auth.save_credentials(cfg.credentials_path, creds)

    # LCD / agent: paired offline until heartbeat succeeds.
    statusio.write_status(
        cfg.status_path,
        statusio.AgentStatus(
            pairing="paired",
            cloud="offline",
            node_id=creds.node_id,
            name=creds.name,
            organization_name=creds.organization_name,
            warehouse_name=creds.warehouse_label(),
            agent_version=AGENT_VERSION,
        ),
    )

    mode = auth.credentials_mode(cfg.credentials_path)
    print("Paired successfully.")
    print(f"  node_id:      {creds.node_id}")
    print(f"  name:         {creds.name or '—'}")
    print(f"  organization: {creds.organization_name or '—'}")
    print(f"  warehouse:    {creds.warehouse_label()}")
    print(f"  credentials:  {cfg.credentials_path} (mode {oct(mode or 0)})")
    print("  (device_token stored; not shown)")
    print("Restart or wait for vesyl-print-agent to heartbeat.")
    return 0


def cmd_enroll(args: argparse.Namespace) -> int:
    cfg = load_config()
    cfg.ensure_dirs()
    write_default_config(cfg.config_path)

    token = args.token.strip()
    if not token:
        _die("enrollment token required")

    client = CloudClient(cfg.api_base_url)
    try:
        data = client.enroll(
            token,
            hostname=sysinfo.hostname(),
            agent_version=AGENT_VERSION,
            platform=default_platform(),
            name=args.name,
        )
    except CloudError as e:
        _die(f"enroll failed: {e.message}" + (f" ({e.code})" if e.code else ""))

    if not data.get("device_token"):
        _die("enroll response missing device_token")

    creds = auth.credentials_from_pair_response(data)
    auth.save_credentials(cfg.credentials_path, creds)
    statusio.write_status(
        cfg.status_path,
        statusio.AgentStatus(
            pairing="paired",
            cloud="offline",
            node_id=creds.node_id,
            name=creds.name,
            organization_name=creds.organization_name,
            warehouse_name=creds.warehouse_label(),
            agent_version=AGENT_VERSION,
        ),
    )
    print("Enrolled successfully.")
    print(f"  node_id:      {creds.node_id}")
    print(f"  organization: {creds.organization_name or '—'}")
    print(f"  warehouse:    {creds.warehouse_label()}")
    print(f"  credentials:  {cfg.credentials_path}")
    print("  (device_token stored; not shown)")
    return 0


def cmd_status(args: argparse.Namespace) -> int:
    cfg = load_config()
    creds = auth.load_credentials(cfg.credentials_path)
    st = statusio.read_status(cfg.status_path)

    print(f"api_base_url:  {cfg.api_base_url}")
    print(f"config:        {cfg.config_path}")
    print(f"credentials:   {cfg.credentials_path}")
    print(f"status file:   {cfg.status_path}")
    print(f"agent_version: {AGENT_VERSION}")
    print()

    if not creds:
        pairing = (st.pairing if st else "unpaired")
        print(f"pairing:       {pairing}")
        if st and st.pairing == "revoked":
            print("  Re-pair required: vesyl-print claim <CODE>")
        else:
            print("  Not paired. Claim with: vesyl-print claim <CODE>")
        if st:
            print(f"cloud:         {st.cloud}")
            if st.last_error:
                print(f"last_error:    {st.last_error}")
        return 0

    print("pairing:       paired (local credentials present)")
    print(f"node_id:       {creds.node_id}")
    print(f"name:          {creds.name or '—'}")
    print(f"organization:  {creds.organization_name or '—'}")
    print(f"warehouse:     {creds.warehouse_label()}")
    mode = auth.credentials_mode(cfg.credentials_path)
    print(f"cred mode:     {oct(mode) if mode is not None else '—'}")

    if st:
        print(f"cloud:         {st.cloud}")
        print(f"last_heartbeat:{st.last_heartbeat_at or '—'}")
        if st.last_error:
            print(f"last_error:    {st.last_error}")

    if args.check:
        client = CloudClient(cfg.api_base_url)
        try:
            who = client.whoami(creds.device_token)
            print()
            print("whoami: OK")
            # Print public fields only
            pub = {
                k: who.get(k)
                for k in ("node_id", "name", "hostname", "status", "warehouse", "organization")
                if k in who
            }
            print(json.dumps(pub, indent=2))
        except CloudError as e:
            print()
            print(f"whoami: FAILED — {e.message}" + (f" ({e.code})" if e.code else ""))
            return 1
    return 0


def cmd_unpair(args: argparse.Namespace) -> int:
    cfg = load_config()
    removed = auth.clear_credentials(cfg.credentials_path)
    statusio.write_status(
        cfg.status_path,
        statusio.AgentStatus(pairing="unpaired", cloud="unknown", agent_version=AGENT_VERSION),
    )
    if removed:
        print(f"Removed credentials at {cfg.credentials_path}")
    else:
        print("No local credentials to remove.")
    print("Local unpair only — cloud node record is unchanged.")
    return 0


def cmd_agent(args: argparse.Namespace) -> int:
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    agent_mod.run_agent(load_config())
    return 0


def cmd_version(args: argparse.Namespace) -> int:
    cfg = load_config()
    install_root = update_mod.resolve_install_root(cfg)
    cur = update_mod.current_release_dir(install_root)
    print(f"agent_version:  {update_mod.package_version()}")
    print(f"platform:       {default_platform()}")
    print(f"install_root:   {install_root}")
    print(f"current_slot:   {cur if cur else '—(running from source tree)'}")
    releases = update_mod.list_releases(install_root)
    if releases:
        print(f"releases:       {', '.join(releases)}")
    ust = update_mod.read_update_status(cfg.update_status_path)
    if ust:
        print(f"update_status:  {ust.status}")
        if ust.target_version:
            print(f"target_version: {ust.target_version}")
        if ust.previous_version:
            print(f"previous_slot:  {ust.previous_version}")
        if ust.health_deadline_at:
            print(f"health_deadline:{ust.health_deadline_at}")
        if ust.last_error:
            print(f"last_error:     {ust.last_error}")
    return 0


def cmd_update(args: argparse.Namespace) -> int:
    cfg = load_config()
    cfg.ensure_dirs()
    action = args.update_action

    if action == "check":
        print(f"current:  {update_mod.package_version()}")
        print(f"channel:  {cfg.update_channel}")
        print(f"releases: {cfg.releases_base_url}")
        print(f"auto:     {cfg.auto_update_enabled}")
        # Optional live check via whoami/heartbeat if paired
        creds = auth.load_credentials(cfg.credentials_path)
        if not creds:
            print("paired:   no — claim first for cloud desired version")
            return 0
        client = CloudClient(cfg.api_base_url)
        try:
            hb = client.heartbeat(
                creds.device_token,
                agent_version=update_mod.package_version(),
                hostname=sysinfo.hostname(),
                platform=default_platform(),
            )
        except CloudError as e:
            _die(f"heartbeat failed: {e.message}")
        desired = hb.get("desired_agent_version") or hb.get("desired_version")
        print(f"desired:  {desired or '—(server did not set desired_agent_version)'}")
        if hb.get("update_url") or hb.get("manifest_url"):
            print(f"manifest: {hb.get('update_url') or hb.get('manifest_url')}")
        if desired and update_mod.version_cmp(str(desired), update_mod.package_version()) != 0:
            print("status:   update available")
            return 0
        print("status:   up to date (or no desired version)")
        return 0

    if action == "apply":
        if args.file:
            # Offline tarball + manifest
            manifest_path = Path(args.manifest) if args.manifest else None
            if not manifest_path or not manifest_path.is_file():
                _die("--manifest PATH required with --file")
            data = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest = update_mod.ReleaseManifest.from_dict(data)
            # Point artifact at local file via file:// or copy path handling
            tarball = Path(args.file).expanduser().resolve()
            if not tarball.is_file():
                _die(f"file not found: {tarball}")
            sha = update_mod.sha256_file(tarball)
            if sha != manifest.artifact_sha256:
                _die(f"sha256 mismatch: file={sha} manifest={manifest.artifact_sha256}")
            try:
                pem = None
                if cfg.update_public_key_path:
                    pem = update_mod.load_public_key_pem(Path(cfg.update_public_key_path))
                else:
                    try:
                        pem = update_mod.load_public_key_pem()
                    except update_mod.UpdateError:
                        if cfg.update_require_signature:
                            _die("no public key; set update_require_signature false for lab")
                require = cfg.update_require_signature and pem is not None
                if require:
                    update_mod.verify_manifest(manifest, public_key_pem=pem)
                install_root = update_mod.resolve_install_root(cfg)
                # Local apply without re-download
                release_dir = install_root / "releases" / manifest.version
                if release_dir.exists():
                    import shutil

                    shutil.rmtree(release_dir)
                update_mod.extract_tarball(tarball, release_dir)
                update_mod.write_version_file(release_dir, manifest.version)
                update_mod.flip_current(install_root, manifest.version)
                print(f"activated {manifest.version} at {install_root / 'current'}")
                if args.restart:
                    update_mod.restart_services()
                    print("services restarted")
            except update_mod.UpdateError as e:
                _die(e.message)
            return 0

        # Online: use heartbeat desired + update_url, or explicit --version
        creds = auth.load_credentials(cfg.credentials_path)
        if not creds and not args.manifest_url:
            _die("not paired and no --manifest-url / --file")
        if args.manifest_url:
            try:
                manifest = update_mod.fetch_manifest(args.manifest_url)
                pem = None
                try:
                    pem = update_mod.load_public_key_pem(
                        Path(cfg.update_public_key_path)
                        if cfg.update_public_key_path
                        else None
                    )
                except update_mod.UpdateError:
                    pem = None
                update_mod.apply_release(
                    manifest,
                    install_root=update_mod.resolve_install_root(cfg),
                    public_key_pem=pem,
                    require_signature=cfg.update_require_signature and pem is not None,
                    restart=args.restart,
                )
                print(f"applied {manifest.version}")
            except update_mod.UpdateError as e:
                _die(e.message)
            return 0

        client = CloudClient(cfg.api_base_url)
        try:
            hb = client.heartbeat(
                creds.device_token,
                agent_version=update_mod.package_version(),
                hostname=sysinfo.hostname(),
                platform=default_platform(),
            )
        except CloudError as e:
            _die(f"heartbeat failed: {e.message}")
        if args.version:
            hb = {
                **hb,
                "desired_agent_version": args.version,
                "update_url": hb.get("update_url")
                or update_mod.default_manifest_url(
                    cfg.releases_base_url, args.version, cfg.update_channel
                ),
            }
        ust = update_mod.maybe_update_from_heartbeat(
            hb, cfg=cfg, auto_apply=True
        )
        update_mod.write_update_status(cfg.update_status_path, ust)
        print(json.dumps(ust.to_dict(), indent=2))
        return 0 if ust.status != "failed" else 1

    if action == "rollback":
        install_root = update_mod.resolve_install_root(cfg)
        try:
            ver = update_mod.rollback(install_root, to_version=args.version)
            print(f"rolled back to {ver}")
            if args.restart:
                update_mod.restart_services()
        except update_mod.UpdateError as e:
            _die(e.message)
        return 0

    _die(f"unknown update action: {action}")
    return 1


def cmd_print_test(args: argparse.Namespace) -> int:
    """Submit a local file through the durable job pipeline (no cloud)."""
    cfg = load_config()
    cfg.ensure_dirs()

    path = Path(args.file).expanduser()
    if not path.is_file():
        _die(f"file not found: {path}")

    queue = args.queue
    if not queue:
        # Prefer first configured CUPS network queue name (not display name).
        nets = printers.configured_network_queues()
        if not nets:
            _die("no CUPS network printers; pass --queue <cups_name>")
        queue = nets[0][0]
        print(f"Using CUPS queue: {queue}")

    try:
        job = jobs.job_from_local_file(
            path,
            queue,
            title=args.title,
            copies=args.copies,
        )
    except jobs.JobError as e:
        _die(e.message)

    store = jobs.store_from_config(cfg)
    print(f"job_id:     {job.id}")
    print(f"file:       {path}")
    print(f"cups_name:  {queue}")
    print(f"queue_dir:  {store.queue_dir}")

    try:
        state = jobs.receive_job(job, store)
    except jobs.JobError as e:
        _die(f"print failed: {e.message} ({e.code})")

    print(f"result:     {state}")
    if store.is_processed(job.id):
        print(f"processed:  {store.processed_path(job.id)}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="vesyl-print",
        description="VESYL print node — claim, enroll, status, agent, print-test, update",
    )
    p.add_argument(
        "--version",
        action="version",
        version=f"vesyl-print {AGENT_VERSION}",
    )
    sub = p.add_subparsers(dest="command", required=True)

    c = sub.add_parser("claim", help="Pair this node with an 8-char claim code")
    c.add_argument("code", help="Claim code (dashes optional)")
    c.add_argument("--name", help="Optional display name for this node")
    c.set_defaults(func=cmd_claim)

    e = sub.add_parser("enroll", help="Pair with a headless enrollment token")
    e.add_argument("token", help="Enrollment token")
    e.add_argument("--name", help="Optional display name for this node")
    e.set_defaults(func=cmd_enroll)

    s = sub.add_parser("status", help="Show local pairing and cloud status")
    s.add_argument(
        "--check",
        action="store_true",
        help="Call whoami against the API",
    )
    s.set_defaults(func=cmd_status)

    u = sub.add_parser("unpair", help="Delete local credentials only")
    u.set_defaults(func=cmd_unpair)

    a = sub.add_parser("agent", help="Run the cloud agent (heartbeat loop)")
    a.add_argument("-v", "--verbose", action="store_true")
    a.set_defaults(func=cmd_agent)

    v = sub.add_parser("version", help="Show agent version and install slots")
    v.set_defaults(func=cmd_version)

    up = sub.add_parser("update", help="Check / apply / rollback app OTA")
    up_sub = up.add_subparsers(dest="update_action", required=True)
    up_sub.add_parser("check", help="Show current vs cloud desired version")
    ap_p = up_sub.add_parser("apply", help="Download+install update")
    ap_p.add_argument("--version", help="Target version (uses releases_base_url)")
    ap_p.add_argument("--manifest-url", help="Direct manifest URL")
    ap_p.add_argument("--file", help="Local release tarball (with --manifest)")
    ap_p.add_argument("--manifest", help="Local manifest JSON (with --file)")
    ap_p.add_argument(
        "--restart",
        action="store_true",
        help="Restart agent/display after apply",
    )
    rb = up_sub.add_parser("rollback", help="Activate previous release slot")
    rb.add_argument("--version", help="Explicit version to roll back to")
    rb.add_argument("--restart", action="store_true")
    up.set_defaults(func=cmd_update)

    pt = sub.add_parser(
        "print-test",
        help="Print a local file via durable queue + CUPS (no cloud)",
    )
    pt.add_argument(
        "--file",
        "-f",
        required=True,
        help="Path to PDF/PNG/JPEG (or any file CUPS accepts)",
    )
    pt.add_argument(
        "--queue",
        "-q",
        help="CUPS queue name (default: first network printer)",
    )
    pt.add_argument("--title", help="Job title for lp -t")
    pt.add_argument(
        "--copies",
        type=int,
        default=1,
        help="Number of copies (default 1)",
    )
    pt.set_defaults(func=cmd_print_test)

    return p


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
