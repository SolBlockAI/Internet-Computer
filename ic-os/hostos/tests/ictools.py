#!/usr/bin/env python3
import atexit
import os
import shutil
import subprocess
import sys
import tempfile
import time

import cbor
import gflags
import requests

FLAGS = gflags.FLAGS

gflags.DEFINE_integer("timeout", 240, "Timeout in seconds to wait for IC to come up")


class ICConfig(object):
    """Store configuration for an instance of the Internet Computer."""

    def __init__(self, workdir, nns_ips, node_subnet_index, root_subnet):
        """Initialize an IC with the given settings."""
        self.workdir = workdir
        self.nns_ips = list(nns_ips)
        self.node_subnet_index = node_subnet_index
        self.root_subnet = root_subnet


def send_upgrade_ssh(host, image, filename):
    command_line = [
        "scp",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "PasswordAuthentication=false",
        image,
        "root@[%s]:%s" % (host, filename),
    ]
    subprocess.run(command_line, check=True)


def apply_upgrade_ssh(host, image):
    command_line = [
        "ssh",
        "-o",
        "ConnectTimeout=1",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ServerAliveInterval=1",
        "-o",
        "PasswordAuthentication=false",
        "-tt",
        "root@%s" % host,
        "/opt/ic/bin/install-upgrade.sh",
        image,
    ]
    subprocess.run(command_line, check=True)


def prep_ssh():
    config = build_ssh_extra_config()["accounts_ssh_authorized_keys"]

    ssh_keys_dir = tempfile.mkdtemp()
    atexit.register(lambda: shutil.rmtree(ssh_keys_dir))
    for account, keyfile in config.items():
        with open(keyfile) as f:
            keys = f.read()
        with open(os.path.join(ssh_keys_dir, account), "w") as f:
            f.write(keys)
    return ssh_keys_dir


def build_config_folder(name, ip, ssh_keys):
    output = tempfile.mkdtemp()
    atexit.register(lambda: shutil.rmtree(output))

    ip_address = "%s/%d" % (ip["address"], ip["mask_length"])
    ip_gateway = ip["gateway"]
    with open(os.path.join(output, "config.ini"), "w") as f:
        f.write("ipv6_address=%s\n" % ip_address)
        f.write("ipv6_gateway=%s\n" % ip_gateway)
        f.write("hostname=%s\n" % name)
    subprocess.run(["cp", "-r", ssh_keys, "%s/ssh_authorized_keys" % output], check=True)
    return output


def build_bootstrap_config_image(name, **kwargs):
    config_image_dir = tempfile.mkdtemp()
    atexit.register(lambda: shutil.rmtree(config_image_dir))
    config_image = os.path.join(config_image_dir, "config-%s.img" % name)

    if "accounts_ssh_authorized_keys" in kwargs:
        accounts_ssh_authorized_keys = kwargs["accounts_ssh_authorized_keys"]
        ssh_keys_dir = tempfile.mkdtemp()
        atexit.register(lambda: shutil.rmtree(ssh_keys_dir))
        for account, keyfile in accounts_ssh_authorized_keys.items():
            with open(keyfile) as f:
                keys = f.read()
            with open(os.path.join(ssh_keys_dir, account), "w") as f:
                f.write(keys)
        kwargs["accounts_ssh_authorized_keys"] = ssh_keys_dir

    bootstrap_script = os.path.join(os.path.dirname(__file__), "..", "scripts", "build-bootstrap-config-image.sh")
    args = [bootstrap_script, config_image]
    for key, value in kwargs.items():
        args.append("--" + key)
        args.append(value)
    subprocess.run(args, stdout=subprocess.DEVNULL, check=True)
    return config_image


def wait_ic_version(replica_url, version, timeout):
    start = time.time()
    now = start
    while now < start + timeout:
        try:
            req = requests.get(replica_url)
            status = cbor.loads(req.content)
            if version == status.value["impl_version"]:
                print("✅ ic-version on %-30s   %s" % (replica_url, version))
                return
        except Exception as e:
            print(e)
            time.sleep(1)
            now = time.time()
    raise TimeoutError("Timeout when waiting for IC version %s." % version)


def wait_host_version(host, target_version, timeout):
    start = time.time()
    now = start
    command_line = [
        "ssh",
        "-o",
        "ConnectTimeout=1",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ServerAliveInterval=1",
        "-o",
        "PasswordAuthentication=false",
        "-tt",
        "root@%s" % host,
        "cat",
        "/boot/version.txt",
    ]
    while now < start + timeout:
        try:
            version = subprocess.run(command_line, check=True, capture_output=True).stdout.decode("utf-8").strip()
            if version == target_version:
                print("✅ host version on %-30s   %s" % (host, version))
                return
        except Exception as e:
            print(e)
            time.sleep(1)
            now = time.time()
    raise TimeoutError("Timeout when waiting for host version %s." % version)


def get_ic_version(replica_url):
    timeout = 5
    start = time.time()
    now = start
    while now < start + timeout:
        try:
            req = requests.get(replica_url)
            status = cbor.loads(req.content)
            return status.value["impl_version"]
        except Exception as e:
            print(e)
            time.sleep(1)
            now = time.time()
    raise TimeoutError("Failed to determine IC version.")


def get_host_version(host):
    timeout = 5
    start = time.time()
    now = start
    while now < start + timeout:
        try:
            command_line = [
                "ssh",
                "-o",
                "ConnectTimeout=1",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ServerAliveInterval=1",
                "-o",
                "PasswordAuthentication=false",
                "-tt",
                "root@%s" % host,
                "cat",
                "/boot/version.txt",
            ]
            version = subprocess.run(command_line, check=True, capture_output=True).stdout.decode("utf-8").strip()
            return version
        except Exception as e:
            print(e)
            time.sleep(1)
            now = time.time()
    raise TimeoutError("Failed to determine host version.")


def ic_prep(subnets, version, root_subnet=0):
    workdir = tempfile.mkdtemp()
    atexit.register(lambda workdir=workdir: shutil.rmtree(workdir))

    nns_ips = []
    nodes = []
    node_index = 0
    subnet_index = 0
    node_subnet_index = []
    for subnet in subnets:
        for ipv6 in subnet:
            nodes.append("--node")
            nodes.append("idx:%d,subnet_idx:%d,p2p_addr:\"[%s]:4100\",xnet_api:\"[%s]:2497\",public_api:\"[%s]:8080\"" % (node_index, subnet_index, ipv6, ipv6, ipv6))
            if subnet_index == root_subnet:
                nns_ips.append(ipv6)
            node_subnet_index.append(subnet_index)
            node_index += 1
        subnet_index += 1

    subprocess.run(
        [
            FLAGS.ic_prep_bin,
            "--working-dir",
            workdir,
            "--replica-version",
            version,
            "--dkg-interval-length",
            "10",
            "--nns-subnet-index",
            "%d" % root_subnet,
        ]
        + nodes,
        check=True,
    )

    return ICConfig(workdir, nns_ips, node_subnet_index, root_subnet)


def build_ic_prep_inject_config(machine, ic_config, index, extra_config={}):
    ipv6 = machine.get_ips(6)[0]
    args = {
        "ipv6_address": "%s/%d" % (ipv6["address"], ipv6["mask_length"]),
        "ipv6_gateway": ipv6["gateway"],
        "nns_url": "http://[%s]:8080" % ic_config.nns_ips[0],
        "nns_public_key": os.path.join(ic_config.workdir, "nns_public_key.pem"),
        "ic_crypto": os.path.join(ic_config.workdir, "node-%d" % index, "crypto"),
    }
    if ic_config.node_subnet_index[index] == ic_config.root_subnet:
        args["ic_registry_local_store"] = os.path.join(ic_config.workdir, "ic_registry_local_store")
    args.update(extra_config)

    return build_bootstrap_config_image(machine.get_name(), **args)


def wait_ic_up(ic_config, timeout=FLAGS.timeout):
    wait_http_up("http://[%s]:8080" % ic_config.nns_ips[0], timeout)


def wait_http_up(url, timeout=FLAGS.timeout):
    start = time.time()
    now = start
    while now < start + timeout:
        try:
            requests.get(url)
            return
        except Exception:
            sys.stderr.write(
                ("Waiting for IC to come up at %s, retrying for next %.1f seconds\n" % (url, start + timeout - now))
            )
            sys.stderr.flush()
            time.sleep(1)
            now = time.time()
    raise TimeoutError("Time out waiting for IC instance to come up.")


def wait_ssh_up(host, timeout=FLAGS.timeout):
    start = time.time()
    now = start
    command_line = [
        "ssh",
        "-o",
        "ConnectTimeout=1",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ServerAliveInterval=1",
        "-o",
        "PasswordAuthentication=false",
        "-tt",
        "root@%s" % host,
        "echo",
        "alive",
    ]
    while now < start + timeout:
        try:
            subprocess.run(command_line, check=True)
            return
        except Exception:
            sys.stderr.write(
                (
                    "Waiting for machine to come up at %s, retrying for next %.1f seconds\n"
                    % (host, start + timeout - now)
                )
            )
            sys.stderr.flush()
            time.sleep(1)
            now = time.time()
    raise TimeoutError("Time out waiting for IC instance to come up.")


def get_upgrade_image_version(image):
    command_line = f'tar xOzf "{image}" --occurrence=1 ./VERSION.TXT || tar xOzf "{image}" --occurrence=1 ./version.txt'

    process = subprocess.Popen(command_line, shell=True, stdout=subprocess.PIPE)
    return process.stdout.read().decode("utf-8").strip()


def build_ssh_extra_config():
    """
    Build extra config containing ssh keys.

    Build an amendent to the IC guest OS bootstrap config that contains
    ssh keys for accessing the node. If there are no ssh keys existing
    yet (this is the case for CI runners), also create ssh keys.
    """
    # Ensure that $HOME/.ssh/id_rsa.pub exists
    home_ssh = os.path.join(os.environ["HOME"], ".ssh")
    id_rsa_pub = os.path.join(home_ssh, "id_rsa.pub")

    if not os.path.exists(home_ssh):
        os.mkdir(home_ssh)
    if not os.path.exists(id_rsa_pub):
        subprocess.run(
            ["ssh-keygen", "-q", "-N", "", "-f", os.path.join(home_ssh, "id_rsa")],
            check=True,
        )

    # Assign keys to root user so we have root login on the node.
    return {
        "accounts_ssh_authorized_keys": {
            "root": id_rsa_pub,
            "backup": id_rsa_pub,
            "readonly": id_rsa_pub,
            "admin": id_rsa_pub,
        }
    }
