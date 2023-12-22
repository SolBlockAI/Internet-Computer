"""
Base implementation for experiment.

This class provides common functionality to a benchmark run:
- Getting IC topology, revision, URLs
- Instrumenting nodes
- Collecting metrics

This class issues API:


WorkloadExperiment inherits BaseExperiment to follow similar workflow of initiation -> start iteration
-> inspect iteration -> ... -> finish
"""
import itertools
import json
import os
import random
import re
import subprocess
import sys
import time
import traceback
from pathlib import Path
from typing import List

import gflags
from retry import retry
from termcolor import colored

from common import ansible, flamegraphs, machine_failure, misc, prometheus, report, ssh

NNS_SUBNET_INDEX = 0  # Subnet index of the NNS subnetwork
MAINNET_NNS_SUBNET_ID = "tdb26-jop6k-aogll-7ltgs-eruif-6kk7m-qpktf-gdiqx-mxtrf-vb5e6-eqe"
MAINNET_NNS_URL = "https://ic0.app"

FLAGS = gflags.FLAGS
gflags.DEFINE_string("testnet", None, 'Testnet to use. Use "mercury" to run against mainnet.')
gflags.MarkFlagAsRequired("testnet")
gflags.DEFINE_string(
    "canister_ids",
    "",
    "Use given canister IDs instead of installing new canisters. Given as JSON dict canister name -> canister.",
)
gflags.DEFINE_string(
    "cache_path", "", "Path to a file that should be used as a cache. Only use if you know what you are doing."
)
gflags.DEFINE_string("artifacts_path", "", "Path to the artifacts directory")
gflags.DEFINE_string("workload_generator_path", "", "Path to the workload generator to be used")
gflags.DEFINE_boolean("no_instrument", False, "Do not instrument target machine")
gflags.DEFINE_string("targets", "", "Set load target IP addresses from this comma-separated list directly.")
gflags.DEFINE_string("top_level_out_dir", "", "Set the top-level output directory. Default is the git commit id.")
gflags.DEFINE_string(
    "second_level_out_dir",
    "",
    "Set the second-level output directory. Default is the UNIX timestamp at benchmark start.",
)

gflags.DEFINE_boolean("simulate_machine_failures", False, "Simulate machine failures while testing.")
gflags.DEFINE_string("nns_url", "", "Use the following NNS URL instead of getting it from the testnet configuration")
gflags.DEFINE_integer("iter_duration", 300, "Duration in seconds to run each iteration of the experiment.")
gflags.DEFINE_string(
    "datapoints",
    "",
    (
        "Datapoints for each iteration. Supported are a) comma-separated floats, b) ranges start-end:step or c) "
        "exponential start~target~end where more measurements are executed the closer we get to target."
    ),
)


def get_artifacts_path():
    if len(FLAGS.artifacts_path) < 1:
        return os.path.join(Path(__file__).parents[2], "artifacts/release")
    else:
        return FLAGS.artifacts_path


class BaseExperiment:
    """Wrapper class around experiments."""

    def __init__(self, request_type="query"):
        """Init."""
        misc.parse_command_line_args()

        self.cached_topologies = {}
        self.artifacts_path = get_artifacts_path()

        self.__load_artifacts()

        self.testnet = FLAGS.testnet

        # Map canister name -> list of canister IDs
        self.canister_ids = {}

        # Used only to track which canister ID to return in case canister IDs have
        # been passed as argument.
        self.canister_id_from_argument_index = {}

        # List of canisters that have been installed
        self.canister = []
        self.metrics = []

        self.t_experiment_start = int(time.time())
        self.iteration = 0

        self.request_type = request_type
        # List of PIDs to wait for when terminating the benchmark
        self.pids_to_finish = []

        self.base_experiment_initialized = False
        self.cache = None

        # Determine if boundary nodes are used in benchmark.
        if (
            FLAGS.targets == "https://ic0.app"
            or FLAGS.targets == "https://ic0.dev"
            or re.search(r"https://icp\d.io", FLAGS.targets)
            or FLAGS.targets.endswith(".testnet.dfinity.network")
        ):
            print(colored("Benchmarking boundary nodes .. reduced function scope", "red"))
            self.benchmark_boundary_nodes = True
        else:
            self.benchmark_boundary_nodes = False

        if self.has_cache():
            try:
                with open(FLAGS.cache_path) as f:
                    print(f"♻️  Cache has been found at {FLAGS.cache_path}.")
                    self.cache = json.loads(f.read())
            except Exception:
                print(f"♻️  Cache {FLAGS.cache_path} did not exist yet, creating ..")
                self.cache = {}

    def get_ic_version(self, m):
        """Retrieve the IC version from the given machine m."""
        from common import ictools

        return ictools.get_ic_version("http://[{}]:8080/api/v2/status".format(m))

    def has_cache(self):
        return len(FLAGS.cache_path) > 0

    def from_cache(self, key):
        if self.has_cache():
            c = self.cache.get(key, None)
            if c is not None:
                c_out = str(c) if len(str(c)) < 100 else str(c)[:100] + ".."
                print(f"♻️  Using cached value: {key} <- {c_out}")
            return c
        return None

    def persist_cache(self):
        if self.has_cache():
            assert len(FLAGS.cache_path) > 0
            with open(FLAGS.cache_path, "w") as f:
                f.write(json.dumps(self.cache))

    def store_cache(self, key, value):
        if self.has_cache():
            previous_value = self.cache.get(key, None)
            if value != previous_value:
                print(f"♻️  Caching: {key} <- {value}")
                self.cache[key] = value
                self.persist_cache()

    def init(self):
        """Initialize experiment."""
        if self.base_experiment_initialized:
            raise Exception("Base experiment has already been initialized, aborting .. ")
        self.base_experiment_initialized = True

        self.git_hash = self.get_ic_version(self.get_machine_to_instrument())
        print(f"Running against an IC with git hash: {self.git_hash}")

        self.out_dir_timestamp = int(time.time())
        self.out_dir = "results/{}/{}/".format(
            self.git_hash if len(FLAGS.top_level_out_dir) < 1 else FLAGS.top_level_out_dir,
            self.out_dir_timestamp if len(FLAGS.second_level_out_dir) < 1 else FLAGS.second_level_out_dir,
        )
        os.makedirs(self.out_dir, 0o755)
        print(f"📂 Storing output in {self.out_dir}")

        self.__store_ic_info()
        self.__store_hardware_info()

    def __load_artifacts(self):
        self.artifacts_hash = misc.load_artifacts(self.artifacts_path)
        self.__set_workload_generator_path()

    def __set_workload_generator_path(self):
        """Set path to the workload generator that should be used for this experiment run."""
        if len(FLAGS.workload_generator_path) > 0:
            self.workload_generator_path = FLAGS.workload_generator_path
        else:
            self.workload_generator_path = os.path.join(self.artifacts_path, "ic-workload-generator")
        print(f"Using workload generator at {self.workload_generator_path}")

    def get_machine_to_instrument(self) -> str:
        return self.get_machines_to_target()[0]

    def get_machines_to_target(self) -> [str]:
        """Return the machine to instrument."""
        if len(FLAGS.targets) > 0:
            return FLAGS.targets.split(",")

        topology = self.__get_topology()
        for subnet, subnet_info in topology["subnets"].items():
            subnet_type = subnet_info["subnet_type"]
            subnet_nodes = subnet_info["nodes"]
            if subnet_type == "application":
                return [node_details["ipv6"] for node_details in subnet_nodes.values()]

    def get_subnet_to_instrument(self) -> str:
        """Return the subnet to instrument."""
        topology = self.__get_topology()
        for subnet, info in topology["subnets"].items():
            subnet_type = info["subnet_type"]
            if subnet_type == "application":
                return subnet

    def run_experiment(self, config):
        """Run a single iteration of the experiment."""
        self.start_iteration()
        result = self.run_experiment_internal(config)
        self.end_iteration(config)
        return result

    def run_experiment_internal(self, config):
        """Run a single iteration of the experiment."""
        raise NotImplementedError()

    def __init_metrics(self):
        """Initialize metrics to collect for experiment."""
        self.metrics = [
            flamegraphs.Flamegraph("flamegraph", self.get_machine_to_instrument(), not FLAGS.no_instrument),
            prometheus.Prometheus("prometheus", self.get_machine_to_instrument(), not FLAGS.no_instrument),
        ]
        for m in self.metrics:
            m.init()

    def init_experiment(self):
        """Initialize what's necessary to run experiments."""
        self.__init_metrics()

    def start_iteration(self):
        """Start a new iteration of the experiment."""
        self.iteration += 1
        self.t_iter_start = int(time.time())
        print("Starting iteration at: ", time.time() - self.t_experiment_start)

        # Create output directory
        self.iter_outdir = "{}/{}".format(self.out_dir, self.iteration)
        os.makedirs(self.iter_outdir, 0o755)

        if FLAGS.simulate_machine_failures:
            machine_failure.MachineFailure(self).start()

        # Start metrics for this iteration
        for m in self.metrics:
            m.start_iteration(self.iter_outdir)

    def end_iteration(self, configuration={}):
        """End a new iteration of the experiment."""
        self.t_iter_end = int(time.time())
        print(
            (
                "Ending iteration at: ",
                time.time() - self.t_experiment_start,
                " - duration:",
                self.t_iter_end - self.t_iter_start,
            )
        )

        for m in self.metrics:
            m.end_iteration(self)

        # Dump experiment info
        with open(os.path.join(self.iter_outdir, "iteration.json"), "w") as iter_file:
            iter_file.write(
                json.dumps(
                    {
                        "t_start": self.t_iter_start,
                        "t_end": self.t_iter_end,
                        "configuration": configuration,
                    }
                )
            )

    def end_experiment(self):
        """End the experiment."""
        print("Waiting for unfinished PIDs")
        for pid in self.pids_to_finish:
            pid.wait()
        for m in self.metrics:
            m.end_benchmark(self)
        print(
            f"Experiment finished. Generating report like: pipenv run common/generate_report.py --base_dir='results/' --git_revision='{self.git_hash}' --timestamp='{self.out_dir_timestamp}'"
        )
        print(f"📂 Experiment output has been stored in: {self.out_dir}")

    def _get_ic_admin_path(self):
        """Return path to ic-admin."""
        return os.path.join(self.artifacts_path, "ic-admin")

    @retry(tries=5)
    def __get_topology(self, nns_url=None):
        """
        Get the current topology from the registry.

        A different NNS can be chosen by setting nns_url. This is useful, for example
        when multiple testnets are used, one for workload generators, and one for
        target machines.
        """
        if nns_url is None:
            nns_url = self._get_nns_url()

        cache_key = f"topology_{nns_url}"
        cached_value = self.from_cache(cache_key)
        if cached_value is not None:
            print(
                f"Returning persisted topology for {nns_url} (this might lead to incorrect results if redeployed since cache has been written)."
            )
            return cached_value

        if nns_url not in self.cached_topologies:
            print(
                f"Getting topology from ic-admin ({nns_url}) - ",
                colored("use --cache_path=/tmp/cache to cache", "yellow"),
            )
            res = subprocess.check_output(
                [self._get_ic_admin_path(), "--nns-url", nns_url, "get-topology"], encoding="utf-8"
            )
            self.cached_topologies[nns_url] = json.loads(res)
            self.store_cache(cache_key, json.loads(res))
        return self.cached_topologies[nns_url]

    def __get_node_info(self, nodeid, nns_url=None):
        """
        Get info for the given node from the registry.

        A different NNS can be chosen by setting nns_url. This is useful, for example
        when multiple testnets are used, one for workload generators, and one for
        target machines.
        """
        if nns_url is None:
            nns_url = self._get_nns_url()
        return subprocess.check_output(
            [self._get_ic_admin_path(), "--nns-url", nns_url, "get-node", nodeid], encoding="utf-8"
        )

    def _get_subnet_info(self, subnet_idx):
        """Get info for the given subnet from the registry."""
        return subprocess.check_output(
            [self._get_ic_admin_path(), "--nns-url", self._get_nns_url(), "get-subnet", str(subnet_idx)],
            encoding="utf-8",
        )

    def __store_ic_info(self):
        """Store subnet info for the subnet that we are targeting in the experiment output directory."""
        try:
            jsondata = self._get_subnet_info(self.get_subnet_to_instrument())
            with open(os.path.join(self.out_dir, "subnet_info.json"), "w") as subnet_file:
                subnet_file.write(jsondata)
        except subprocess.CalledProcessError as e:
            if self.has_cache():
                print(colored(f"Failed to get subnet info. Retry after deleting cache file {FLAGS.cache_path}", "red"))
                raise e

        jsondata = self.__get_topology()
        with open(os.path.join(self.out_dir, "topology.json"), "w") as subnet_file:
            subnet_file.write(json.dumps(jsondata, indent=2))

    def __store_hardware_info(self):
        """Store info for the target machine in the experiment output directory."""
        if FLAGS.no_instrument:
            return
        for (cmd, name) in [("lscpu", "lscpu"), ("free -h", "free"), ("df -h", "df"), ("uname -r", "uname")]:
            self.pids_to_finish.append(
                ssh.run_ssh(
                    self.get_machine_to_instrument(),
                    cmd,
                    f_stdout=os.path.join(self.out_dir, f"{name}.stdout.txt"),
                    f_stderr=os.path.join(self.out_dir, f"{name}.stderr.txt"),
                )
            )

    def get_node_ip_address(self, nodeid, nns_url=None):
        """Get HTTP endpoint for the given node."""
        nodeinfo = self.__get_node_info(nodeid, nns_url)
        ip = re.findall(r'ip_addr: "([a-f0-9:A-F]+)"', nodeinfo)
        return ip[0]

    def get_nodeoperator_of_node(self, nodeid):
        """Get node operator entry in node record."""
        nodeinfo = self.get_node_info(nodeid)
        no = re.findall(r"node_operator_id: (\[.+?\])", nodeinfo)
        return no[0]

    def get_unassigned_nodes(self):
        """Return a list of unassigned node IDs in the given subnetwork."""
        topo = self.__get_topology()
        return list(topo["unassigned_nodes"].keys())

    def get_subnets(self):
        """Get the currently running subnetworks."""
        topo = self.__get_topology()
        return list(topo["subnets"].keys())

    def get_subnet_members(self, subnet_index):
        """Get members of subnet with the given subnet index (not subnet ID)."""
        topo = self.__get_topology()
        subnet_info = list(topo["subnets"].values())
        return subnet_info[subnet_index]["membership"]

    @retry(tries=5)
    def get_mainnet_nns_ip(self):
        """Get NNS IP address on mainnet."""
        topology = self.__get_topology(nns_url=MAINNET_NNS_URL)
        for subnet, subnet_info in topology["subnets"].items():
            if subnet == MAINNET_NNS_SUBNET_ID:
                subnet_nodes_ipv6 = [node["ipv6"] for node in subnet_info["nodes"].values()]
                nns_ip = random.choice(subnet_nodes_ipv6)
                print(f"Using NNS ip address: {nns_ip}")
                return nns_ip
        raise Exception(f"Failed to get the mainnet NNS url from {MAINNET_NNS_URL}")

    def _get_nns_ip(self):
        if FLAGS.testnet == "mercury":
            return self.get_mainnet_nns_ip()
        else:
            return ansible.get_ansible_hostnames_for_subnet(FLAGS.testnet, NNS_SUBNET_INDEX, sort=False)[0]

    def _get_nns_url(self):
        """
        Get the testnets NNS url.

        The NNS url can either be specified by a command line flag, the mainnet NNS url can be
        used the ansible configuration files can be parsed for benchmarking testnets.
        """
        if len(FLAGS.nns_url) > 0:
            return FLAGS.nns_url
        ip = self._get_nns_ip()
        return f"http://[{ip}]:8080"

    def add_node_to_subnet(self, subnet_index, node_ids):
        """Add nodes given in node_ids to the given subnetwork."""
        assert isinstance(node_ids, list)
        processes = []
        for node_id in node_ids:
            cmd = [
                self._get_ic_admin_path(),
                "--nns-url",
                self._get_nns_url(),
                "propose-to-add-nodes-to-subnet",
                "--test-neuron-proposer",
                "--subnet-id",
                str(subnet_index),
                "--summary",
                "'Adding nodes to subnet'",
                node_id,
            ]
            print(f"Executing {cmd}")
            p = subprocess.Popen(cmd)
            processes.append(p)

        for p in processes:
            p.wait()

        num_tries = 0
        node_added = False
        while node_added:

            print(f"Testing if node {node_id} is a member of subnet {subnet_index}")
            num_tries += 1
            assert num_tries < 10  # Otherwise timeout

            node_added = True

            for node_id in node_ids:
                node_added &= node_id in self.get_subnet_members(subnet_index)

    def install_canister_nonblocking(self, target, canister=None):
        """
        Install the canister on the given machine.

        Note that canisters are currently always installed as best effort.
        """
        print("Installing canister .. ")
        cmd = [self.workload_generator_path, "http://[{}]:8080".format(target), "-n", "1", "-r", "0"]
        if canister is not None:
            cmd += ["--canister", canister]
        return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def install_canister(self, target: str = None, canister=None, check=True) -> str:
        """
        Install the canister on the machine given by IPv6 address.

        Note that canisters are currently always installed as best effort.

        Returns the canister ID if installation was successful.
        """
        if target is None:
            target = self.get_machine_to_instrument()
        print(f"Installing canister .. {canister} on {target}")
        this_canister = canister if canister is not None else "counter"
        this_canister_name = "".join(this_canister.split("#")[0])

        if FLAGS.canister_ids is not None and len(FLAGS.canister_ids) > 0:
            canister_id_map_as_json = json.loads(FLAGS.canister_ids)
            print(f"⚠️  Not installing canister, using {FLAGS.canister_ids} ")
            self.canister = list(canister_id_map_as_json.keys())
            self.canister_ids = canister_id_map_as_json
            # Attempt to return a useful canister ID in case of using canisters parsed as argument.
            this_idx = self.canister_id_from_argument_index.get(this_canister_name, 0)
            assert this_canister_name in self.canister_ids
            assert this_idx < len(self.canister_ids[this_canister_name])
            cid = self.canister_ids[this_canister_name][this_idx]
            this_idx += 1
            self.canister_id_from_argument_index[this_canister_name] = this_idx
            return cid

        this_canister_id = None

        cmd = [self.workload_generator_path, f"http://[{target}]:8080", "-n", "1", "-r", "0"]
        if this_canister_name != "counter":
            canister_in_artifacts = os.path.join(self.artifacts_path, f"../canisters/{this_canister_name}.wasm")
            canister_in_artifacts_gz = os.path.join(self.artifacts_path, f"../canisters/{this_canister_name}.wasm.gz")
            canister_in_repo = os.path.join("canisters", f"{this_canister_name}.wasm")
            canister_in_repo_gzip = os.path.join("canisters", f"{this_canister_name}.wasm.gz")
            print(
                f"Looking for canister at locations: {canister_in_artifacts}, {canister_in_artifacts_gz} and {canister_in_repo}")
            if os.path.exists(this_canister):
                cmd += this_canister
            elif os.path.exists(canister_in_artifacts):
                cmd += ["--canister", canister_in_artifacts]
            elif os.path.exists(canister_in_artifacts_gz):
                cmd += ["--canister", canister_in_artifacts_gz]
            elif os.path.exists(canister_in_repo):
                cmd += ["--canister", canister_in_repo]
            elif os.path.exists(canister_in_repo_gzip):
                cmd += ["--canister", canister_in_repo_gzip]
            else:
                cmd += ["--canister", this_canister]
        try:
            p = subprocess.run(
                cmd,
                check=check,
                capture_output=True,
            )
            wg_output = p.stdout.decode("utf-8").strip()
            for line in wg_output.split("\n"):
                canister_id = re.findall(r"Successfully created canister at URL [^ ]*. ID: [^ ]*", line)
                if len(canister_id):
                    cid = canister_id[0].split()[7]
                    if this_canister not in self.canister_ids:
                        self.canister_ids[this_canister] = []
                    self.canister_ids[this_canister].append(cid)
                    self.canister = list(set(self.canister + [this_canister]))
                    this_canister_id = cid
                    print("Found canister ID: ", cid)
                    print(
                        "Canister(s) installed successfully, reuse across runs: ",
                        colored(
                            f"--canister_ids='{json.dumps(self.canister_ids)}'",
                            "yellow",
                        ),
                    )
            wg_err_output = p.stderr.decode("utf-8").strip()
            for line in wg_err_output.split("\n"):
                if "The response of a canister query call contained status 'rejected'" not in line:
                    print("Output (stderr):", line)
        except Exception as e:
            traceback.print_stack()
            print(f"Failed to install canister, return code: {e.returncode}")
            print(f"Command was: {cmd}")
            print(e.output.decode("utf-8"))
            print(e.stderr.decode("utf-8"))
            exit(5)

        return this_canister_id

    def get_hostnames(self, for_subnet_idx=0, nns_url=None):
        """Return hostnames of all machines in the given testnet and subnet from the registry."""
        topology = self.__get_topology(nns_url)
        for curr_subnet_idx, subnet_info in enumerate(topology["subnets"].values()):
            subnet_type = subnet_info["subnet_type"]
            subnet_nodes = subnet_info["nodes"]
            assert curr_subnet_idx != 0 or subnet_type == "system"
            if for_subnet_idx == curr_subnet_idx:
                return sorted([node_details["ipv6"] for node_details in subnet_nodes.values()])

    def get_app_subnet_hostnames(self, nns_url=None, idx=-1):
        """
        Return hostnames of application subnetworks.

        If no subnet index is given as idx, all machines in given
        testnet that are part of an application subnet from the given
        registry will be returned.

        Otherwise, all machines from the given subnet are going to be
        returned.
        """
        ips = []
        topology = self.__get_topology(nns_url)
        for curr_subnet_idx, subnet_info in enumerate(topology["subnets"].values()):
            subnet_type = subnet_info["subnet_type"]
            subnet_nodes = subnet_info["nodes"]
            if (subnet_type != "system" and idx < 0) or (idx >= 0 and curr_subnet_idx == idx):
                ips += [node_details["ipv6"] for node_details in subnet_nodes.values()]
        return sorted(ips)

    def _build_summary_file(self):
        """
        Build dictionary to be used to build the summary file.

        This is overridden by workload experiment, so visibility needs to be _ not __.
        """
        return {}

    def write_summary_file(
        self, experiment_name, experiment_details, xlabels, xtitle="n.a.", rtype="query", state="running"
    ):
        """
        Write the current summary file.

        The idea is that we write one after each iteration, so that we can
        generate reports from intermediate versions.
        """
        # Attempt to parse experiment details to check "schema"
        _ = report.parse_experiment_details(experiment_details)

        d = self._build_summary_file()
        d.update(
            {
                "xlabels": xlabels,
                "xtitle": xtitle,
                "command_line": sys.argv,
                "experiment_name": experiment_name,
                "experiment_details": experiment_details,
                "type": rtype,
                "workload": self.canister,
                "testnet": self.testnet,
                "user": subprocess.check_output(["whoami"], encoding="utf-8"),
                "canister_id": json.dumps(self.canister_ids),
                "artifacts_githash": self.artifacts_hash,
                "t_experiment_start": self.t_experiment_start,
                "t_experiment_end": int(time.time()),
                "state": state,
            }
        )
        with open(os.path.join(self.out_dir, "experiment.json"), "w") as iter_file:
            iter_file.write(json.dumps(d, indent=4))

    def get_iter_logs_from_targets(self, machines: List[str], since_time: str, outdir: str):
        """Fetch logs from target machines since the given time."""
        if FLAGS.no_instrument:
            return
        ssh.run_all_ssh_in_parallel(
            machines,
            [f"journalctl -u ic-replica --since={since_time}" for m in machines],
            outdir + "/replica-log-{}-stdout.txt",
            outdir + "/replica-log-{}-stderr.txt",
        )

    def get_canister_ids(self):
        """Return canister IDs of all canisters installed by the suite."""
        return list(itertools.chain.from_iterable([k for _, k in self.canister_ids.items()]))

    @staticmethod
    def get_datapoints(default: [float]) -> [float]:
        """Parse datapoints given as arguments or otherwise return default."""
        if len(FLAGS.datapoints) > 0:
            d = misc.parse_datapoints(FLAGS.datapoints)
        else:
            d = default
        print(f"Using datapoints: {d}")
        return d
