#!/usr/bin/env python3
"""
Statesync experiment

Purpose: Stress state sync mechanisms with large state changes

Minimal topology: 13 node app subnet, 1 node NNS

Runbook:
. Deploy `num_canister` instances of statesync canister
. Make an update call
. Kill a node for > 3 * DKG interval update calls
. Grow canisters `num_canister_grow` many times
. Kill nodes in the same data center
. Restart node
. Make another update call

Measure and determine:
  State sync duration
  Flamegraphs

For testing purposes, deploy testnets with "--dkg-interval-length 14" to avoid overly long
runtimes.
"""
import json
import math
import os
import random
import sys
import time
import traceback
from multiprocessing import Pool

import gflags
from termcolor import colored

sys.path.append(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import common.base_experiment as base_experiment  # noqa
import common.misc as misc  # noqa
import common.prometheus as prometheus  # noqa
from common.machine_failure import MachineFailure  # noqa

CANISTER = "statesync-test-canister"

# Number of canisters to install in each iteration
FLAGS = gflags.FLAGS
gflags.DEFINE_integer("subnet_index", 1, "index of the subnet to target")
gflags.DEFINE_integer("num_canisters", 1, "number of canisters to install")
gflags.DEFINE_integer("num_canister_grow", 0, "number of expansions")
gflags.DEFINE_integer("batch_size", 1, "number of parallel expansions")
gflags.DEFINE_boolean(
    "grow_only",
    False,
    "Only grow the subnet state but do not trigger a state sync. For ad-hoc experiments with large state",
)


def expand_single_state(t):
    (hostname, canister_id, index, seed) = t
    agent = misc.get_agent(hostname)
    response = agent.update_raw(canister_id, "expand_state", json.dumps([index, seed]))
    print("response", response)


class StatesyncExperiment(base_experiment.BaseExperiment):
    def __init__(self):
        """Initialize the state sync experiment."""
        super().__init__()
        super().init()
        super().init_experiment()
        hostname = self.get_node_ip_address(self.get_subnet_members(FLAGS.subnet_index)[0])
        for i in range(FLAGS.num_canisters):
            self.install_canister(hostname, canister=CANISTER)
        print("Successfully installed", FLAGS.num_canisters, "canisters.")

    def __change_state(hostname, canister_id, seed):
        agent = misc.get_agent(hostname)
        response = agent.update_raw(canister_id, "change_state", json.dumps(seed))
        print("response", response)

    def change_state(hostnames: [str], canister_ids: [str], seed):
        """
        Make an update call to the statesync canister to change the state.

        We allow individual calls to fail, since we mostly care about the timing here.
        """
        for (hostname, canister_id) in zip(hostnames, canister_ids):
            try:
                StatesyncExperiment.__change_state(hostname, canister_id, seed)
            except Exception as e:
                print(f"State change called failed: {e} - continuing")

    def expand_state(hostnames: [str], canister_ids: [str], index, seed):
        """Make an update call to the statesync canister to expand the state."""
        random.seed(seed)

        def extend(t):
            (h, c) = t
            return (h, c, index, random.randint(0, 2 ** 32 - 1))

        arguments = list(map(extend, zip(hostnames, canister_ids)))
        with Pool(FLAGS.batch_size) as p:
            p.map(expand_single_state, arguments)

    def run_experiment_internal(self, config):
        print("Resolve members, IPs and dkg interval length for subnet 1...")
        members = self.get_subnet_members(FLAGS.subnet_index)
        error_msg = "Need at least 2 nodes in the subnet for this experiment.\n"
        error_msg += (
            "The experiment is targeting the subnet with index "
            + str(FLAGS.subnet_index)
            + ", which contains "
            + str(len(members))
            + " nodes."
        )
        assert len(members) > 2, error_msg

        assert FLAGS.num_canister_grow <= 8, "Parameter num_canister_grow must be in the range [0,8]"

        nodes = [self.get_node_ip_address(node) for node in members]
        info = json.loads(self._get_subnet_info(1))
        dkg_len = info["records"][0]["value"]["dkg_interval_length"] + 1
        print("Dkg length", dkg_len)
        print("Make a change state call to canister", self.get_canister_ids(), "at node", nodes[0])
        StatesyncExperiment.change_state([nodes[0]] * FLAGS.num_canisters, self.get_canister_ids(), 0)

        if not FLAGS.grow_only:
            print("Stop node with ip...", nodes[-1])
            MachineFailure.kill_nodes([nodes[-1]])
            print()

        if dkg_len > 50:
            print(
                colored(
                    "Large DKG length detected. For interactive debugging, try deploying with --dkg-interval-length 14",
                    "yellow",
                )
            )

        for i in range(FLAGS.num_canister_grow):
            StatesyncExperiment.expand_state([nodes[0]] * FLAGS.num_canisters, self.get_canister_ids(), i, i)
            print("Expand state call number", i + 1, "out of", FLAGS.num_canister_grow)

        if FLAGS.grow_only:
            return 0

        print("Make 3*dkg_len change state calls to canister", self.get_canister_ids()[0], "at node", nodes[0])
        for i in range(3 * dkg_len):
            # Each change_state update call has a latency of at least one block,
            # so calling it dkg_len times takes at least dkg_len blocks.
            # Doing those calls instead of sleeping ensure that the benchmark is decoupled from
            # the subnetwork configuration (specifically the finalization rate)
            StatesyncExperiment.change_state([nodes[0]], self.get_canister_ids(), i)
            print("State change call number", i, "out of", 3 * dkg_len)

        # Kill all other nodes in the same data center to avoid local communication to catch-up
        # from restarted node.
        restarted_ip_prefix = nodes[-1][:9]
        print("Stop nodes with the same prefix", restarted_ip_prefix)
        same_prefix_nodes = [node for node in nodes[:-1] if node[:9] == restarted_ip_prefix]
        error_msg = "When stopping too many nodes, the network will not be able to make progress.\n"
        error_msg += (
            "The experiment is targeting the subnet with index "
            + str(FLAGS.subnet_index)
            + ", which contains "
            + str(len(members))
            + " nodes.\n"
        )
        error_msg += (
            "At most " + str(math.floor((len(members) - 1) / 3)) + " nodes can be stopped without halting the subnet."
        )
        assert 3 * len(same_prefix_nodes) < len(members), error_msg
        count = 0
        for node in same_prefix_nodes:
            MachineFailure.kill_nodes([node])
            print("Stopped node", node)
            count += 1
        print("Stopped", count, "nodes")

        print("Restart the first node we stopped earlier")
        MachineFailure.start_nodes([nodes[-1]])
        print("Restarted, sleep 60s")
        time.sleep(60)

        print("Try to access restarted node...")
        for i in range(30):
            try:
                self.get_ic_version(nodes[-1])
                print("Success after", i + 1, "attempts")
                break
            except TimeoutError:
                print(traceback.format_exc())
            time.sleep(5)

        print("Make another call, send it to the restarted node")
        StatesyncExperiment.change_state([nodes[-1]], self.get_canister_ids(), i)

        state_sync_duration = prometheus.get_state_sync_duration(self.testnet, [nodes[-1]], int(time.time()))
        parsed = list(prometheus.parse(state_sync_duration))
        assert len(parsed) <= 1
        if len(parsed) == 1:
            ((value_timestamp, value), _metric) = parsed[0]
            print("state sync duration is:", value)
            state_sync_duration = value
            print("*************")
        else:
            state_sync_duration = None

        print("End of test")
        print("Cleanup: restart nodes with the same prefix", restarted_ip_prefix)
        count = 0
        for node in nodes[:-1]:
            if node[:9] == restarted_ip_prefix:
                MachineFailure.start_nodes([node])
                print("Restart node", node)
                count += 1
        print("Restarted", count, "nodes")
        print()

        return state_sync_duration


if __name__ == "__main__":

    misc.parse_command_line_args()
    exp = StatesyncExperiment()

    state_sync_duration = exp.run_experiment({})

    exp.write_summary_file(
        "run_statesync_experiment",
        {"state_sync_duration": state_sync_duration, "iter_duration": -1},
        ["n.a."],
        "no axis label",
    )

    exp.end_experiment()
