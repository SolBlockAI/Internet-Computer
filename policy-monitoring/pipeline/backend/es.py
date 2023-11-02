import pprint
import re
import sys
from datetime import datetime, timedelta
from typing import Dict, Iterator, List, Optional, Union

from elasticsearch import Elasticsearch, exceptions
from pipeline.alert import AlertService
from pipeline.es_doc import EsDoc
from util.print import assert_with_trace, eprint

from .group import Group


class EsException(Exception):
    pass


def is_replica_log_index(index_name: str) -> bool:
    return index_name.startswith("ic-logs-")


class Es:

    # This array represents a lexicographical order.
    # To scroll through ES pages, we need two factors:
    # 1. Timestamps, ascending (note that their order
    #    is a global invariant, so the point-in-time
    #    concept is not needed here).
    # 2. Unique IDs, descending (could be ascending,
    #    probably) -- TODO
    _SORTER = [
        {"@timestamp": {"order": "asc"}},
        # {"_id", {"order" : "desc"}}
    ]

    # The default number of Elasticsearch documents in one page
    _DEFAULT_PAGE_SIZE = 10_000

    @staticmethod
    def _bookmark(last_hit) -> List[Union[None, float, int, str]]:
        # [last_hit[0], str(last_hit[1])]
        return [last_hit[0]]

    stat: Dict[str, Dict[str, int]]

    def __init__(self, es_url: str, alert_service: AlertService, mainnet: bool, fail=False):
        self.es_url = es_url
        self.es = Elasticsearch(es_url, max_retries=5, timeout=60.0)  # in seconds
        self.alert_service = alert_service
        self.stat = {"raw_logs": dict()}
        self.mainnet = mainnet
        self.fail = fail

    @staticmethod
    def _precise_query(tag: str):
        return {"match": {"tags": {"query": tag, "operator": "and", "fuzziness": "0"}}}

    @staticmethod
    def _time_slice_query(minutes_ago: int):
        return {"range": {"@timestamp": {"gte": f"now-{minutes_ago}m", "lt": "now"}}}

    # TODO: support richer queries, e.g.:
    # @staticmethod
    # def _time_slice_from_a_to_b_query():
    #     return {"range": {"@timestamp": {"gte": "2022-08-04T05:23:58.997Z", "lt": "2022-08-04T08:23:58.997Z"}}}

    def find_testnet_indices(self, tag: str) -> List[str]:
        """
        Find a list of (non-empty) ES indices that are tagged with [tag]
        Exceptions:
            - [self.fail] ==> [EsException] if COUNT query fails for some index
        """
        result = []
        index: str
        for index in filter(is_replica_log_index, self.es.indices.get_alias(index="*")):
            body = {"query": Es._precise_query(tag)}
            try:
                response = self.es.count(index=index, body=body)
            except exceptions.TransportError as e:
                # Should not be raised
                msg = (
                    f"WARNING: ES did not respond to COUNT query "
                    f"for index {index}\n"
                    f"request body: {pprint.pformat(body)}\n"
                    f"exception:\n{str(e)}\n"
                )
                eprint(msg)
                if self.fail:
                    raise EsException(e)
                self.alert_service.alert(
                    text=msg,
                    short_text=f"ES COUNT query failed for {index}",
                )

            size = int(response["count"])
            if size > 0:
                eprint(f"Found index {index} with {size} documents tagged {tag}")
                result.append(index)
                # Save statistics:
                # total number of raw log messages sent to Elasticsearch
                if tag in self.stat["raw_logs"]:
                    # Multiple ES indices containing logs tagged with this group
                    # This happens, e.g., when a test spans beyond one date
                    self.stat["raw_logs"][tag] += size
                else:
                    self.stat["raw_logs"][tag] = size

        return result

    @staticmethod
    def _get_relevant_dates(minutes_ago: int) -> List[datetime]:
        """Returns datetime objects for each day starting [minutes_ago] until now."""
        end_time = datetime.today()
        dates = []
        time = end_time - timedelta(minutes=minutes_ago)
        while time < end_time:
            eprint(f"~ adding {time.strftime('%Y.%m.%d')} to the list of relevant dates")
            dates.append(time)
            time += timedelta(days=1)
        return dates

    @staticmethod
    def _is_index_relevant(index: str, dates: List[datetime]) -> bool:
        """Check if this index is relevant, i.e., it contains logs produced starting [minutes_ago] until now."""
        for date in dates:
            # Example: ic-logs-2023.08.23
            m = re.match(r"ic-logs-(\d\d\d\d\.\d\d\.\d\d)", index)
            if (
                m
                and len(m.groups()) == 1
                and m.group(1) == date.strftime("%Y.%m.%d")
            ):
                return True

        return False

    def find_mainnet_inidices(self, window_minutes: int) -> List[str]:
        """
        Find mainnet filebeat indices for the past [num_days]
        Exceptions:
            - [self.fail] ==> [EsException] if COUNT query fails for some index
        """
        dates = self._get_relevant_dates(window_minutes)
        body = {"query": Es._time_slice_query(window_minutes)}
        result = []
        index: str
        for index in self.es.indices.get_alias(index="*"):
            if not self._is_index_relevant(index, dates):
                continue

            response = None
            try:
                response = self.es.count(index=index, body=body)
            except exceptions.TransportError as e:
                # Should not be raised
                msg = (
                    f"WARNING: ES did not respond to COUNT query "
                    f"for MAINNET index {index}\n"
                    f"exception:\n{str(e)}\n"
                )
                if self.fail:
                    raise EsException(e)
                self.alert_service.alert(
                    text=msg,
                    short_text="ES COUNT query failed for a MAINNET index",
                )
            assert response, f"Elasticsearch index {index} is not available from endpoint {self.es_url}"
            size = int(response["count"])
            if size > 0:
                eprint(f"Found MAINNET index {index} with {size} documents")
                result.append(index)

                # Save statistics:
                # total number of raw log messages sent to Elasticsearch
                tag = f"mainnet--{index}"
                assert_with_trace(tag not in self.stat["raw_logs"], f"duplicate mainnet index {index}")
                self.stat["raw_logs"][tag] = size
            else:
                msg = f"WARNING: MAINNET index {index} is empty"
                eprint(msg)
                self.alert_service.alert(
                    text=msg,
                    short_text="MAINNET index is empty",
                )

        if len(result) == 0:
            raise EsException("Could not find any ES indices with MAINNET documents.")
        return result

    def _get_logs_for_group(self, group_name: str, limit: int, window_minutes: Optional[int]) -> Iterator[EsDoc]:

        if self.mainnet:
            assert_with_trace(window_minutes is not None, "[window_minutes] must be specified since Es.mainnet is true")
            upper_bound = 24 * 60
            assert_with_trace(window_minutes <= upper_bound, f"[window_minutes] should not exceed {upper_bound}")  # type: ignore
            indices = self.find_mainnet_inidices(window_minutes)  # type: ignore
        else:
            indices = self.find_testnet_indices(tag=group_name)
            if len(indices) == 0:
                msg = (
                    f"Could not find any ES indices with documents tagged `{group_name}`. "
                    f"Try repeating this script in a few minutes if the hourly tests "
                    f"have started recently)"
                )
                eprint(msg)
                self.alert_service.alert(
                    level="🕳️",
                    text=msg,
                    short_text=f"could not find ES index for {group_name}",
                )
                return

        if limit > 0:
            page_size = min(Es._DEFAULT_PAGE_SIZE, limit)
        else:
            page_size = Es._DEFAULT_PAGE_SIZE

        eprint("\nStarting to collect logs from ES ...")
        eprint(f". = {page_size} events", flush=True)

        for i, doc in enumerate(
            self.es_doc_stream(indices, tag=group_name, page_size=page_size, window_minutes=window_minutes)
        ):
            yield EsDoc(doc)
            if limit > 0 and i == limit:
                eprint("\n", flush=True)
                break

        eprint(f"\nObtained {i + 1} entries from ES")

    def download_logs(self, groups: Dict[str, Group], limit_per_group: int, minutes_per_group: Optional[int]):
        """
        Downloads logs corresponding to each of the [groups]. Details:
            - At most [limit_per_group] entries are downloaded. [limit_per_group] == 0 means no limit.
            - If [minutes_per_group] is defined, only download logs emitted within the [now-minutes_per_group; now] window.
        Exceptions:
            - [EsException] if logs could not be downloaded due to ES API issues
        """
        for group_name in groups:
            try:
                groups[group_name].logs = self._get_logs_for_group(group_name, limit_per_group, minutes_per_group)
            except EsException as e:
                self.alert_service.alert(
                    level="🧀",
                    text="Elasticsearch exception:\n```\n%s\n```" % str(e),
                    short_text="Exception from Elasticsearch",
                )
                continue

    def es_doc_stream(
        self, indices: List[str], tag: str, page_size=20, window_minutes: Optional[int] = None
    ) -> Iterator:

        if self.mainnet:
            assert_with_trace(window_minutes is not None, "[window_minutes] must be specified since Es.mainnet is true")
            query = Es._time_slice_query(window_minutes)  # type: ignore
        else:
            query = Es._precise_query(tag)

        try:
            response = self.es.search(index=indices, size=page_size, sort=Es._SORTER, query=query)  # type: ignore
        except exceptions.TransportError as e:
            msg = (
                "ES query failed.\n"
                "If your system tests have started recently, try repeating "
                "this script in a few minutes.\n"
            )
            self.alert_service.alert(
                text=msg,
                short_text=f"ES COUNT query failed for {','.join(indices)}",
            )
            raise EsException(e)

        docs = response["hits"]["hits"]

        if not docs:
            raise EsException(f"No ES documents were found for tag `{tag}` in indices `{','.join(indices)}`")

        for doc in docs:
            yield doc

        last_hit = docs[-1]["sort"]

        while True:
            sys.stderr.write(".")
            sys.stderr.flush()

            bookmark = Es._bookmark(last_hit)

            response = self.es.search(
                index=indices, size=page_size, sort=Es._SORTER, query=query, search_after=bookmark  # type: ignore
            )
            docs = response["hits"]["hits"]

            if not docs:
                sys.stderr.write("\n")
                sys.stderr.flush()
                break

            for doc in docs:
                yield doc

            last_hit = docs[-1]["sort"]
