"""vesyl-print CLI: claim, enroll, status, unpair, agent."""

from __future__ import annotations

import argparse
import json
import logging
import sys

import agent as agent_mod
import auth
import statusio
import sysinfo
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
            warehouse_name=creds.warehouse_name,
            agent_version=AGENT_VERSION,
        ),
    )

    mode = auth.credentials_mode(cfg.credentials_path)
    print("Paired successfully.")
    print(f"  node_id:      {creds.node_id}")
    print(f"  name:         {creds.name or '—'}")
    print(f"  organization: {creds.organization_name or '—'}")
    print(f"  warehouse:    {creds.warehouse_name or '—'}")
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
            warehouse_name=creds.warehouse_name,
            agent_version=AGENT_VERSION,
        ),
    )
    print("Enrolled successfully.")
    print(f"  node_id:      {creds.node_id}")
    print(f"  organization: {creds.organization_name or '—'}")
    print(f"  warehouse:    {creds.warehouse_name or '—'}")
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
    print(f"warehouse:     {creds.warehouse_name or '—'}")
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


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="vesyl-print",
        description="VESYL print node — claim, enroll, status, agent",
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

    return p


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
