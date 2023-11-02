#!/usr/bin/env python
"""
Purpose: Measure IC performance give a complex workload.

The workload configuration to use is being read from a separate workload description file.
"""
import math
import os
import shutil
import sys

import gflags
import toml

sys.path.append(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import common.misc as misc  # noqa
import common.workload_experiment as workload_experiment  # noqa
import common.workload as workload  # noqa
import common.report as report  # noqa
from workloads.hooks.workload_hooks import WorkloadHooks  # noqa

FLAGS = gflags.FLAGS
gflags.DEFINE_string("workload", None, "Workload description to execute")
gflags.MarkFlagAsRequired("workload")

gflags.DEFINE_integer("initial_rps", 100, "Starting number for requests per second.")
gflags.DEFINE_integer("increment_rps", 50, "Increment of requests per second per round.")
gflags.DEFINE_integer(
    "max_rps", 40000, "Maximum requests per second to be sent. Experiment will wrap up beyond this number."
)
gflags.DEFINE_float("scale_duration", -1, "Scale all durations by the given factor. Ignored if negative.")

NUM_MACHINES_PER_WORKLOAD = 1  # TODO - make configurable in toml


def get_hooks(workload_file: str) -> WorkloadHooks:
    """
    Find workload hooks for the given workload file.

    When adding custom experiment hooks to workloads/hooks, the hook needs to be associated with the experiment in this method.
    """
    from workloads.hooks.xrc_hooks import XrcHooks  # noqa

    hooks = {"xrc.toml": XrcHooks()}
    return hooks.get(workload_file, None)


class MixedWorkloadExperiment(workload_experiment.WorkloadExperiment):
    """Logic for mixed workload experiments."""

    def __init__(self):
        """Install canisters."""
        super().__init__()
        self.workload_description = []
        self.hooks = get_hooks(os.path.basename(FLAGS.workload))
        print(f"Using hooks from: {self.hooks}")
        shutil.copy(FLAGS.workload, self.out_dir)
        with open(FLAGS.workload) as f:
            self.raw_description = toml.loads(f.read())
            self.install_canister_from_workload_description(self.raw_description)
            self.workload_description = workload.workload_description_from_dict(self.raw_description, self.canister_ids)

        is_all_queries = True
        for w in self.workload_description:
            if w.method is not None and w.method.lower() != "query":
                is_all_queries = False

        if not is_all_queries:
            # Make sure we target all replicas in benchmark
            # Previously, for query calls we would target only a single node, which we want to avoid for mixed workload experiments.
            self.request_type = "call"

    def install_canister_from_workload_description(self, description):
        """Install all canisters required to run the given workload description."""
        if "install_canisters" in description:
            for extra_canister in description["install_canisters"]:
                print(f"Installing extra canister {extra_canister} from workload description file")
                self.install_canister(canister=extra_canister)

        for wl in description["workload"]:
            canister = wl["canister"]
            if canister not in self.canister_ids:
                self.install_canister(canister=canister)

    def run_experiment_internal(self, config):
        """Run workload generator with the load specified in config."""
        f_stdout = os.path.join(self.iter_outdir, "workload-generator-{}.stdout.txt")
        f_stderr = os.path.join(self.iter_outdir, "workload-generator-{}.stderr.txt")

        threads = []  # Array of type: [workload.Workload]
        curr_workload_generator_index = 0
        for wl_idx, wl in enumerate(self.workload_description):
            print(wl)
            rps = int(math.ceil(config["load_total"] * wl.rps_ratio))
            if FLAGS.scale_duration > 0:
                scaled_duration = int(wl.duration * FLAGS.scale_duration)
                wl = wl._replace(duration=scaled_duration)
            if wl.rps < 0:
                wl = wl._replace(rps=rps)
            if isinstance(wl.raw_payload, list):
                raw_payload = wl.raw_payload[config["iteration"] % len(wl.raw_payload)]
                wl = wl._replace(raw_payload=raw_payload)
            load_generators = []
            if len(self.machines) < 1:
                raise Exception("No machines for load generation, aborting")
            for _ in range(NUM_MACHINES_PER_WORKLOAD):
                load_generators.append(self.machines[curr_workload_generator_index])
                curr_workload_generator_index = (curr_workload_generator_index + 1) % len(self.machines)

            print(f"Generating workload for machines {load_generators}")
            load = workload.Workload(
                load_generators,
                self.target_nodes,
                wl,
                wl_idx,
                self.iter_outdir,
                f_stdout,
                f_stderr,
            )
            load.start()
            threads.append(load)

        for thread in threads:
            thread: workload.Workload = thread
            workload_description = thread.workload
            thread.join()
            destinations = thread.fetch_results()
            print(f"Evaluating results from {workload_description} machines")
            report.evaluate_summaries(destinations)

    def run_iterations(self, iterations=None):
        """Exercise the experiment with specified iterations."""
        for i, d in enumerate(iterations):
            if self.hooks is not None:
                self.hooks.iteration_hook(self, i)
            print(f"🚀 Running with total load {d}")
            config = {"load_total": d, "iteration": i}
            self.run_experiment(config)

        if self.hooks is not None:
            self.hooks.experiment_done_hook(self)

        self.write_summary_file(
            "run_mixed_workload_experiment",
            {
                "is_update": FLAGS.use_updates,
                "labels": [
                    f"{d.get('canister', '')} - "
                    f"{d.get('rps_ratio', '')}% rps with "
                    f"{d.get('arguments', '')} @"
                    f"{d.get('start_delay', 0)}s for {d.get('duration', '')}s"
                    for d in self.raw_description["workload"]
                ],
                "iter_duration": max(
                    [
                        d.get("duration", workload.get_default_workload_duration())
                        for d in self.raw_description["workload"]
                    ]
                ),
                "description": self.raw_description["description"],
                "title": self.raw_description["title"],
            },
            iterations,
            "base requests / s",
            "mixed",
        )


if __name__ == "__main__":
    exp = MixedWorkloadExperiment()
    iterations = exp.get_datapoints(
        misc.get_iterations(FLAGS.target_rps, FLAGS.initial_rps, FLAGS.max_rps, FLAGS.increment_rps, 2)
    )
    print(f"🚀 Running with iterations: {iterations}")
    exp.run_iterations(iterations)
    exp.end_experiment()
