import gflags
import os
import sys

sys.path.append(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from common import workload_experiment # noqa
from common import misc # noqa

FLAGS = gflags.FLAGS
{{#unless use_workload_generators}}
gflags.DEFINE_string("echo_string", "Hello world", "String to print")
{{/unless}}


class {{experiment_name}}(workload_experiment.WorkloadExperiment):
    """Example for how to call an external binary from the scalability suite."""

    {{#if install_canister}}
    def init_experiment(self):
        super().init_experiment()
        self.install_canister(self.target_nodes[0], "{{canister_name}}")
    {{/if}}

    def run_experiment_internal(self, config):
        """Run the experiment."""
        # TODO: Add logic that executes one iteration of your benchmark.
        # TODO: Most benchmarks run multiple iterations, with increasing
        # TODO: Load between iterations.
        
        {{#if use_workload_generators}}
        # TODO: Set correct arguments in workload generator
        return self.run_workload_generator(
            self.machines, # List of machines that the workload generator should run on
            self.target_nodes, # List of IC nodes running the canister that should be targeted
            config["load_total"], # Number of requests per second to execute
            canister_ids=None, # None = Target all installed canisters
            duration=FLAGS.iter_duration, # How long to run the workload (in secs)
            payload=None, # Payload to send to the canister
            method=None, # Update or query, None = QueryCounter
            call_method=None, # Name of the caniter's method to call, works only iff method=Update or method=Query
            arguments=[] # List of extra-arguments to the workload generator
        )
        {{else}}
        # TODO: replace with commands you want to run in each iteration
        subprocess.check_output(["echo", config["string"], config["iteration"]])
        {{/if}}


    def run_iterations(self, datapoints=None):
        """Exercise the experiment with specified iterations."""
        for load_total in datapoints:
            evaluated_summaries = super().run_experiment({
                    "load_total": load_total,
                })

            p99 = evaluated_summaries.percentiles[99]
            failure_rate = evaluated_summaries.failure_rate
            print(f"load {load_total}: p99 latency: {p99} - failure rate {failure_rate}")

        self.write_summary_file(
            "{{experiment_fname}}",
            {
                # TODO - experiment specific output for you benchmark (arbitrary)
                {{#if use_workload_generators}}
                {{else}}
                "string": FLAGS.echo_string
                {{/if}}
            },
            [],     # TODO - x-axis labels on plots (e.g. the request rates)
            "n.a."  # TODO - x-axis title on plots
        )


if __name__ == "__main__":

    exp = {{experiment_name}}()

    exp.run_experiment({
        {{#if use_workload_generators}}
        "load_total": 20,
        {{else}}
        "string": FLAGS.echo_string,
        "iteration": i,
        {{/if}}
    })

