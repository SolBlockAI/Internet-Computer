#!/usr/bin/env python3
import json
import logging
import math
import os
import statistics
import sys
import traceback
from collections import Counter

import gflags
import pybars
from termcolor import colored

sys.path.append(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from common import misc  # noqa
from common import ansible  # noqa
from common import report  # noqa

FLAGS = gflags.FLAGS
gflags.DEFINE_string(
    "base_dir",
    "./",
    "The base directory where output artifacts are generated into. The base_dir should contain sub folder named with git_revision.",
)
gflags.DEFINE_string(
    "git_revision",
    None,
    "Hash of the git revision which the benchmark run was exercised on. Output folder with this git_revision in name will be used to generate report.",
)
gflags.MarkFlagAsRequired("git_revision")
gflags.DEFINE_string(
    "timestamp",
    None,
    "The timestamp the benchmark run was marked with. Output folder with this timestamp in name will be used to generate report.",
)
gflags.MarkFlagAsRequired("timestamp")
gflags.DEFINE_string("asset_root", "", "Path to the root of the asset canister")
gflags.DEFINE_boolean("strict", False, "Fail generating reports if something is missing")


# When failure rate is below this level, we consider the experiment successful.
ALLOWABLE_FAILURE_RATE = 0.2

# When median latency is below this level, we consider the experiment successful.
ALLOWABLE_LATENCY = 5000


def parse_prometheus(prometheus_path: str, evaluated_summaries):
    """Parse the given prometheus data."""
    http_request_duration = None
    finalization_rate = None
    metrics = None
    if os.path.exists(prometheus_path):
        with open(prometheus_path) as prometheus_metrics:
            try:
                metrics = json.loads(prometheus_metrics.read())
                try:
                    request_duration = statistics.mean(
                        map(float, filter(lambda x: x != "NaN", metrics["http_request_duration"]))
                    )
                    http_request_duration.append(request_duration)
                except Exception as err:
                    print(f"Failed to determine HTTP request duration for file {prometheus_path} - {err}")

                t_start = int(metrics["http_request_rate"][0][0][0])
                xdata = [int(x) - t_start for x, _ in metrics["http_request_rate"][0]]
                ydata = [float(y) for _, y in metrics["http_request_rate"][0]]

                if evaluated_summaries:
                    sorted_histograms = sorted(evaluated_summaries.get_success_rate_histograms())
                    plots = [
                        {"x": xdata, "y": ydata, "name": "HTTP handler"},
                        {
                            "x": [s for s, _ in sorted_histograms],
                            "y": [s for _, s in sorted_histograms],
                            "name": "aggregated workload generator stats",
                        },
                    ]

                    layout = {
                        "yaxis": {"title": "rate [requests / s]", "range": [0, 1.2 * max(ydata)]},
                        "xaxis": {"title": "iteration time [s]"},
                    }

                    metrics.update(
                        {
                            "http_request_rate_plot": plots,
                            "http_request_rate_layout": layout,
                        }
                    )

                if "finalization_rate" in metrics:
                    finalization_rate = metrics["finalization_rate"][1]

            except Exception as err:
                traceback.print_exc()
                print(colored(f"Failed to parse prometheus.json file for {prometheus_path} - {err}", "red"))
    return (http_request_duration, metrics, finalization_rate)


def parse_workload_generator_commands(base_dir: str):
    wg_commands = ""
    i = 0
    run = True
    while run:
        i += 1
        try:
            with open(os.path.join(base_dir, f"workload-generator-cmd-{i}")) as wg_cmd_file:
                wg_commands += wg_cmd_file.read() + "\n"
        except Exception:
            run = False
    return [
        "./ic-workload-generator {}".format(c)
        for c in wg_commands.split("./ic-workload-generator")
        if len(c.strip()) > 0
    ]


def add_plot(name: str, xlabel: str, ylabel: str, x: [str], plots: [([str], str)]):
    """Return a dictionary representing the given plot for templating."""
    plot_data = []
    for (e_y, e_name) in plots:
        data = [(_x, _y) for _x, _y in zip(x, e_y) if not math.isnan(float(_y))]
        x_out = [_x for _x, _ in data]
        y_out = [_y for _, _y in data]
        plot_data.append({"y": y_out, "x": x_out, "name": e_name})

    return {
        f"plot-{name}": plot_data,
        f"layout-{name}": {
            "yaxis": {
                "title": ylabel,
                "rangemode": "tozero",
                "autorange": "true",
            },
            "xaxis": {"title": xlabel},
        },
    }


def resolve_ip_addresses(ips: [str], testnet: str):
    load_generators = []
    country = {
        "fr": "🇩🇪",
        "sf": "🇺🇸",
    }
    print(ips)
    try:
        for machine in ips:
            host = ansible.get_host_for_ip(testnet, machine)
            if host:
                host_prefix = host[:2]
                load_generators.append(
                    {"name": machine, "host": host, "country": country[host_prefix] if host_prefix in country else ""}
                )
            else:
                load_generators.append({"name": machine, "host": "n.a.", "country": "n.a."})
    except Exception:
        traceback.print_exc()
    return load_generators


def add_file(base, path, alt, strict=None):
    if strict is None:
        strict = FLAGS.strict
    content = ""
    try:
        for p in path:
            content += open(os.path.join(base, p)).read()
    except Exception:
        if not strict:
            content += alt
        else:
            raise

    return content


def add_toml_files(base):
    result = []
    for f in [os.path.join(base, f) for f in os.listdir(base) if f.endswith(".toml")]:
        with open(f) as f:
            result.append(f.read())
    return {"toml": result}


def parse_experiment_json(base_path, data):
    """Parse contents of experiment.json and add to output for rendering."""
    try:
        with open(os.path.join(base_path, "experiment.json")) as experiment_info:
            experiment = json.loads(experiment_info.read())
            data.update({"experiment": experiment})
            return experiment
    except Exception:
        print("Failed to parse experiment.json file")
        traceback.print_exc()
        exit(1)


def update_rps_max(prev_rps_max, evaluated_summaries, duration):
    rps_max = prev_rps_max
    avg_succ_rate = evaluated_summaries.get_avg_success_rate(duration)
    latency = evaluated_summaries.percentiles[95] if evaluated_summaries.num_success > 0 else sys.float_info.max
    if evaluated_summaries.failure_rate < ALLOWABLE_FAILURE_RATE and latency < ALLOWABLE_LATENCY:
        if avg_succ_rate > prev_rps_max:
            rps_max = avg_succ_rate
    return rps_max


def generate_report(base, githash, timestamp):
    """Generate report for the given measurement."""
    source = open("templates/experiment.html.hb", mode="r").read()
    data = {
        "iterations": [],
        "timestamp": timestamp,
        "githash": githash,
    }

    http_request_duration = []
    wg_http_latency = []
    wg_http_latency_99 = []
    wg_failure_rate = []
    wg_failure_rates = []
    wg_summary_files = []
    finalization_rates = []

    experiment = parse_experiment_json(base, data)
    rps_max = 0

    # Parse data for each iteration
    # --------------------------------------------------
    for i in sorted([int(i) for i in os.listdir(base) if i.isnumeric()]):
        path = os.path.join(base, str(i))
        if os.path.isdir(path):
            iter_data = {}
            print("Found measurement iteration {} in {}".format(i, path))

            # Workload generator summaries
            aggregated_rates = 0
            t_median_agg = -1
            failure_rate = -1

            files = [
                os.path.join(path, f)
                for f in os.listdir(path)
                if f.startswith("summary_machine_") or f.startswith("summary_workload_")
            ]
            print("Workload generator summary files: ", files)
            evaluated_summaries = None
            if len(files) > 0:
                files = sorted(files)
                evaluated_summaries = report.evaluate_summaries(files)
                (
                    failure_rate,
                    t_median,
                    t_average,
                    t_max,
                    t_min,
                    t_percentile,
                    total_requests,
                    _,
                    _,
                ) = evaluated_summaries.convert_tuple()
                wg_summary_files.append(files)

                experiment_details = report.parse_experiment_details(experiment["experiment_details"])
                aggregated_rates += evaluated_summaries.get_avg_success_rate(experiment_details.iter_duration)

                from statistics import mean

                t_median_agg = mean(t_median)
                t_average_agg = max(t_average)
                t_max_agg = max(t_max)
                t_min_agg = max(t_min)
                rps_max = update_rps_max(rps_max, evaluated_summaries, experiment_details.iter_duration)

                iter_data.update(
                    {
                        "header": i,
                        "failure_rate": "{:.2f}".format(failure_rate * 100),
                        "failure_rate_color": "green" if failure_rate < 0.01 else "red",
                        "t_median": "{:.2f}".format(t_median_agg),
                        "t_average": "{:.2f}".format(t_average_agg),
                        "t_99": "{:.2f}".format(t_percentile[99]),
                        "t_95": "{:.2f}".format(t_percentile[95]),
                        "t_90": "{:.2f}".format(t_percentile[90]),
                        "t_max": "{:.2f}".format(t_max_agg),
                        "t_min": "{:.2f}".format(t_min_agg),
                        "total_requests": total_requests,
                    }
                )

                wg_http_latency.append(t_median)
                wg_http_latency_99.append(t_percentile[99])
                wg_failure_rate.append(failure_rate * 100)
                wg_failure_rates.append(evaluated_summaries.failure_rates)

            print(f"Aggregated succ rate: {aggregated_rates} - {t_median_agg} - {failure_rate}")

            # Search for flamegraph
            flamegraph = [os.path.join(str(i), f) for f in os.listdir(path) if f.startswith("flamegraph_")]
            print("Flamegraph is: ", flamegraph)
            if len(flamegraph) > 0:
                iter_data.update({"flamegraph": flamegraph[0]})

            iter_data["wg_commands"] = parse_workload_generator_commands(path)

            # Iteration configuration
            try:
                with open(os.path.join(path, "iteration.json")) as iteration_conf:
                    iter_data["configuration"] = json.loads(iteration_conf.read())

            except Exception as err:
                print("Failed to parse iteration.json file for iteration {} - {}".format(i, err))

            # Prometheus report
            prometheus_path = os.path.join(path, "prometheus.json")
            p_http_request_duration, p_metrics, p_finalization_rate = parse_prometheus(
                prometheus_path, evaluated_summaries
            )
            if p_http_request_duration is not None:
                http_request_duration.append(p_http_request_duration)
            if p_metrics is not None:
                iter_data.update({"prometheus": p_metrics})
            if p_finalization_rate is not None:
                finalization_rates.append(p_finalization_rate)

            data["iterations"].append(iter_data)

    experiment_name = data["experiment"]["experiment_name"]
    experiment_template_file = "templates/{}.html.hb".format(experiment_name)
    print("Experiment template file is: {}".format(experiment_template_file))
    experiment_source = open(experiment_template_file, mode="r").read()

    if "load_generator_machines" in data["experiment"]:
        data["experiment"]["load_generator_machines"] = resolve_ip_addresses(
            data["experiment"]["load_generator_machines"], data["experiment"]["wg_testnet"]
        )
    if "target_machines" in data["experiment"]:
        data["experiment"]["target_machines"] = resolve_ip_addresses(
            data["experiment"]["target_machines"], data["experiment"]["wg_testnet"]
        )

    compiler = pybars.Compiler()
    template = compiler.compile(source)

    experiment_template = compiler.compile(experiment_source)
    experiment_data = data["experiment"]
    experiment_data["experiment_details"]["rps_max"] = "{:.1f}".format(rps_max)

    # Update experiment.json with rps_max
    with open(os.path.join(base, "experiment.json"), "r") as experiment_file:
        j = json.loads(experiment_file.read())
        j["experiment_details"]["rps_max"] = rps_max
        updated = json.dumps(j, indent=4)
    with open(os.path.join(base, "experiment.json"), "w") as experiment_file:
        experiment_file.write(updated)

    experiment_data.update(add_toml_files(base))

    logging.debug("Rendering experiment details with: ", json.dumps(experiment_data, indent=2))
    experiment_details = experiment_template(experiment_data)

    data.update(
        {
            "experiment-details": experiment_details,
        }
    )

    exp = data["experiment"]
    plots = [(http_request_duration, "http duration")]
    data.update(add_plot("http-latency", exp["xtitle"], "latency [s]", exp["xlabels"], plots))

    wg_summaries = [
        [fname.split("/")[-1].replace("summary_machine_", "") for fname in fnames] for fnames in wg_summary_files
    ]

    if len(wg_failure_rates) > 0:

        # Generate one plot for the failure rate of each workload generator
        # For each of the failure rates, we need to find out which workload generator this failure rate is from.
        # The order in which they are stored in the list should be the same in each round thanks for
        # sorting the workload generators summaries for each round.
        num_workload_generators = len(wg_failure_rates[0])
        for workload_generator_id in range(num_workload_generators):
            workload_generators_idx_in_iterations = [
                list_of_hosts[workload_generator_id] for list_of_hosts in wg_summaries
            ]
            counts = Counter(workload_generators_idx_in_iterations)
            # Each of those should have only one entry, otherwise, the order of failure rates
            # for the workload generators isn't the same in each iteration!
            assert len(counts) == 1

            # With that, we can then also determine the label:
            workload_generator_label = list(counts.elements())[0]

            plots.append(
                (
                    [x[workload_generator_id] * 100.0 for x in wg_failure_rates],
                    f"{workload_generator_label}",
                )
            )
        plots.append((wg_failure_rate, "aggregated failure rate"))

    data.update(add_plot("wg-failure-rate", exp["xtitle"], "failure rate [%]", exp["xlabels"], plots))

    plots = []
    if len(wg_http_latency) > 0:

        num_workload_generators = len(wg_http_latency[0])
        for workload_generator_id in range(num_workload_generators):
            workload_generators_idx_in_iterations = [host[workload_generator_id] for host in wg_summaries]
            counts = Counter(workload_generators_idx_in_iterations)
            # Each of those should have only one entry
            assert len(counts) == 1

            # With that, we can then also determine the label:
            workload_generator_label = list(counts.elements())[0]

            # This seems to be used in the workload generator for invalid requests?
            # Need to be careful with floating point arithmetic when comparing int to float
            def filter_or_minus_one(x):
                INVALID = 18446744073709552000000
                return -1 if abs(x - INVALID) < 0.0000001 else x

            plots.append(
                (
                    [filter_or_minus_one(x[workload_generator_id]) for x in wg_http_latency],
                    f"median {workload_generator_label}",
                )
            )

    plots.append((wg_http_latency_99, "mean 99th percentile of all"))
    data.update(add_plot("wg-http-latency", exp["xtitle"], "latency [ms]", exp["xlabels"], plots))

    if len(finalization_rates) == len(exp["xlabels"]):
        data.update(
            add_plot(
                "finalization-rate",
                exp["xtitle"],
                "finalization rate [1/s]",
                exp["xlabels"],
                [(finalization_rates, "")],
            )
        )
    elif len(finalization_rates) > 0:
        print(
            colored(
                "Not enough data points for finalization rate {}, need {}".format(
                    len(finalization_rates),
                    len(exp["xlabels"]),
                ),
                "red",
            )
        )

    data["lscpu"] = add_file(base, ["lscpu.stdout.txt"], "lscpu data missing")
    data["free"] = add_file(base, ["free.stdout.txt"], "free data missing")
    data["subnet_info"] = add_file(base, ["subnet_info.json"], "subnet info data missing")
    data["topology"] = add_file(base, ["topology.json"], "topology data missing")

    if len(FLAGS.asset_root) > 0:
        report_file = os.path.join(FLAGS.asset_root, FLAGS.git_revision, FLAGS.timestamp, "report.html")
    else:
        report_file = os.path.join(base, "report.html")

    with open(report_file, "w") as outfile:
        logging.debug("Rendering report with: ", json.dumps(data, indent=2))
        data.update({"is_external": len(FLAGS.asset_root) > 0})
        output = template(data)
        outfile.write(output)

    print("Report is at file://{}/{}".format(os.getcwd(), report_file))


if __name__ == "__main__":
    misc.parse_command_line_args()
    base = f"{FLAGS.base_dir}/{FLAGS.git_revision}/{FLAGS.timestamp}"
    generate_report(base, FLAGS.git_revision, FLAGS.timestamp)
