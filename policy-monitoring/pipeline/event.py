from abc import abstractmethod
from typing import Iterable, Optional, Tuple

from .es_doc import EsDoc, RegistryDoc, ReplicaDoc
from .global_infra import GlobalInfra


class Event:
    doc: Optional[EsDoc]  # None is used in case the event is synthetic
    name: str

    def __init__(self, name: str, doc: Optional[EsDoc]):
        self.name = name
        self.doc = doc

    def __str__(self) -> str:
        """Returns serialized representation of this event instance"""
        return f"{type(self).__name__}(name={self.name}, doc={str(self.doc)})"

    def filter(self) -> bool:
        """Indicates whether [self.doc] is relevant"""
        return True

    def unix_ts(self) -> int:
        assert self.doc is not None, "cannot read unix_ts without Event.doc"
        return self.doc.unix_ts()

    @abstractmethod
    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        ...

    def compile(self) -> Iterable[str]:
        if self.filter():
            params_iter = self.compile_params()
            assert params_iter is not None, f"params_iter is None for {str(self)}"
            for params in params_iter:
                if any(map(lambda x: not isinstance(x, str), params)):
                    import pprint

                    str_repr = pprint.pformat(params)
                    print(f"WARNING: unexpected parameter sequence in {str(self)}: {str_repr}")
                yield "@{unix_ts} {predicate}({arguments})\n".format(
                    unix_ts=self.unix_ts(), predicate=self.name, arguments=", ".join(params)
                )


class FinalEvent(Event):
    """Synthetic event"""

    def __init__(self, unix_ts: int):
        super().__init__(name="end_test", doc=None)
        self._unix_ts = unix_ts

    def unix_ts(self) -> int:
        return self._unix_ts

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        return [()]


class RebootEvent(Event):
    doc: EsDoc

    def __init__(self, doc: EsDoc):
        super().__init__(name="reboot", doc=doc)

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        host_addr = self.doc.host_addr()
        if not host_addr or not self.doc.is_host_reboot():
            return []
        else:
            data_center_prefix = GlobalInfra.get_host_dc(host_addr)
            return [(str(host_addr), str(data_center_prefix))]


class RebootIntentEvent(Event):
    doc: EsDoc

    def __init__(self, doc: EsDoc):
        super().__init__(name="reboot_intent", doc=doc)

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        host_addr = self.doc.host_addr()
        if not host_addr or not self.doc.is_host_reboot_intent():
            return []
        else:
            data_center_prefix = GlobalInfra.get_host_dc(host_addr)
            return [(str(host_addr), str(data_center_prefix))]


class InfraEvent(Event):
    def __init__(self, name: str, doc: Optional[EsDoc], infra: GlobalInfra):
        super().__init__(name=name, doc=doc)
        self.infra = infra


class OriginalSubnetTypePreambleEvent(InfraEvent):
    """Synthetic event"""

    def __init__(self, infra: GlobalInfra):
        super().__init__(name="original_subnet_type", doc=None, infra=infra)

    def unix_ts(self) -> int:
        return 0

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        subnet_types = self.infra.get_original_subnet_types()
        return list(subnet_types.items())


class OriginallyInIcPreambleEvent(InfraEvent):
    """Synthetic event"""

    def __init__(self, infra: GlobalInfra):
        super().__init__(name="originally_in_ic", doc=None, infra=infra)

    def unix_ts(self) -> int:
        return 0

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        in_ic_nodes = self.infra.get_original_nodes()
        bla = [(node_id, str(node_addr)) for node_addr, node_id in in_ic_nodes.items()]
        return bla


class OriginallyInSubnetPreambleEvent(InfraEvent):
    """Synthetic event"""

    def __init__(self, infra: GlobalInfra):
        super().__init__(name="originally_in_subnet", doc=None, infra=infra)

    def unix_ts(self) -> int:
        return 0

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        in_subnet_rel = self.infra.get_original_subnet_membership()
        return list(map(lambda p: (p[0], str(self.infra.get_host_ip_addr(p[0])), p[1]), in_subnet_rel.items()))


class RegistryEvent(Event):

    doc: RegistryDoc

    def __init__(self, doc: EsDoc, mutation: str):
        super().__init__(name=f"registry__{mutation}", doc=doc)

    def filter(self) -> bool:
        if super().filter() and self.doc.is_registry_canister():
            self.doc = RegistryDoc(self.doc.repr)
            return True
        else:
            return False


class RegistryEventWithInfra(InfraEvent):

    doc: RegistryDoc

    def __init__(self, doc: EsDoc, mutation: str, infra: GlobalInfra):
        super().__init__(name=f"registry__{mutation}", doc=doc, infra=infra)

    def filter(self) -> bool:
        if super().filter() and self.doc.is_registry_canister():
            self.doc = RegistryDoc(self.doc.repr)
            return True
        else:
            return False


class RegistrySubnetEvent(RegistryEvent):
    def __init__(self, doc: EsDoc, verb: str):
        super().__init__(mutation=f"subnet_{verb}", doc=doc)


class RegistrySubnetCreatedEvent(RegistrySubnetEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(doc=doc, verb="created")

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_created_subnet()
        if params is None:
            return []
        else:
            return [(params.subnet_id, params.subnet_type)]


class RegistrySubnetUpdatedEvent(RegistrySubnetEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(doc=doc, verb="updated")

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_updated_subnet()
        if params is None or params.subnet_type is None:
            return []
        else:
            return [(params.subnet_id, params.subnet_type)]


class RegistryNodeEvent(RegistryEventWithInfra):
    doc: RegistryDoc

    def __init__(self, doc: EsDoc, verb: str, infra: GlobalInfra):
        super().__init__(mutation=f"node_{verb}_subnet", doc=doc, infra=infra)


class RegistryNodesRemovedFromSubnetEvent(RegistryNodeEvent):
    def __init__(self, doc: EsDoc, infra: GlobalInfra):
        super().__init__(doc=doc, verb="removed_from", infra=infra)

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params_iter = self.doc.get_removed_nodes_from_subnet_params()
        if params_iter is None:
            return []
        else:
            return list(
                map(lambda params: (params.node_id, str(self.infra.get_host_ip_addr(params.node_id))), params_iter)
            )


class RegistryNodeAddedToSubnetEvent(RegistryNodeEvent):
    def __init__(self, doc: EsDoc, infra: GlobalInfra):
        super().__init__(doc=doc, verb="added_to", infra=infra)

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params_iter = self.doc.get_added_nodes_to_subnet_params()
        if params_iter is None:
            return []
        else:
            return list(
                map(
                    lambda params: (
                        params.node_id,
                        str(self.infra.get_host_ip_addr(params.node_id)),
                        params.subnet_id,
                    ),
                    params_iter,
                )
            )


class RegistryNodesRemovedFromIcEvent(RegistryEventWithInfra):
    def __init__(self, doc: EsDoc, infra: GlobalInfra):
        super().__init__(doc=doc, mutation="node_removed_from_ic", infra=infra)

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params_iter = self.doc.get_removed_nodes_from_ic_params()
        if params_iter is None:
            return []
        else:
            return list(
                map(
                    lambda params: (
                        params.node_id,
                        str(self.infra.get_host_ip_addr(params.node_id)),
                    ),
                    params_iter,
                )
            )


class RegistryNodeAddedToIcEvent(RegistryEventWithInfra):
    def __init__(self, doc: EsDoc, infra: GlobalInfra):
        super().__init__(doc=doc, mutation="node_added_to_ic", infra=infra)

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_added_node_to_ic_params()
        if params is None:
            return []
        else:
            return [
                (
                    params.node_id,
                    str(self.infra.get_host_ip_addr(params.node_id)),
                )
            ]


class ReplicaEvent(Event):
    doc: ReplicaDoc
    crate: str
    module: str
    node_id: str
    subnet_id: str

    WILDCARD = "*"

    def __init__(self, name: str, doc: EsDoc, crate: str, module: str):
        super().__init__(name=name, doc=doc)
        self.crate = crate
        self.module = module

    def filter(self) -> bool:
        if not (super().filter() and self.doc.is_replica()):
            return False
        else:
            self.doc = ReplicaDoc(self.doc.repr)

            if self.crate == ReplicaEvent.WILDCARD and self.module == ReplicaEvent.WILDCARD:
                return True
            else:
                crate, module = self.doc.get_crate_module()
                if self.crate == ReplicaEvent.WILDCARD:
                    return module == self.module
                if self.module == ReplicaEvent.WILDCARD:
                    return crate == self.crate
                return (crate, module) == (self.crate, self.module)


class NodeMembershipEvent(ReplicaEvent):
    def __init__(self, doc: EsDoc, verb: str):
        super().__init__(name="p2p__node_%s" % verb, doc=doc, crate="ic_p2p", module="download_management")
        self.verb = verb

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_p2p_node_params(verb=self.verb)
        if not params:
            return []
        else:
            return [
                (
                    self.doc.get_node_id(),
                    self.doc.get_subnet_id(),
                    str(params.node_id),  # NOT the ID of the event reported node
                )
            ]


class NodeAddedEvent(NodeMembershipEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(doc, verb="added")


class NodeRemovedEvent(NodeMembershipEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(doc, verb="removed")


class ValidatedBlockProposalEvent(ReplicaEvent):
    def __init__(self, doc: EsDoc, verb: str):
        super().__init__(
            name=f"validated_BlockProposal_{verb}", doc=doc, crate="ic_artifact_manager", module="processors"
        )
        self.verb = verb

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_validated_block_proposal_params(self.verb)
        if not params:
            return []
        else:
            return [(self.doc.get_node_id(), self.doc.get_subnet_id(), params.block_hash)]


class ValidatedBlockProposalAddedEvent(ValidatedBlockProposalEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(doc, verb="Added")


class ValidatedBlockProposalMovedEvent(ValidatedBlockProposalEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(doc, verb="Moved")


class DeliverBatchEvent(ReplicaEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(name="deliver_batch", doc=doc, crate="ic_consensus", module="batch_delivery")

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_batch_delivery_params()
        if not params:
            return []
        else:
            return [
                (
                    self.doc.get_node_id(),
                    self.doc.get_subnet_id(),
                    str(params.block_hash),
                )
            ]


class ConsensusFinalizedEvent(ReplicaEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(name="consensus_finalized", doc=doc, crate="ic_consensus", module="consensus")

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_consensus_finalized_params()
        if not params:
            return []
        else:
            return [
                (
                    self.doc.get_node_id(),
                    self.doc.get_subnet_id(),
                    str(int(params.is_state_available)),
                    str(int(params.is_key_available)),
                )
            ]


class MoveBlockProposalEvent(ReplicaEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(name="move_block_proposal", doc=doc, crate="ic_artifact_manager", module="processors")

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_proposal_moved_params()
        if not params:
            return []
        else:
            return [(self.doc.get_node_id(), self.doc.get_subnet_id(), params.block_hash, params.signer)]


class ControlPlaneSpawnAcceptTaskTlsServerHandshakeFailedEvent(ReplicaEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(
            name="ControlPlane__spawn_accept_task__tls_server_handshake_failed",
            doc=doc,
            crate="ic_transport",
            module="control_plane",
        )

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_control_plane_spawn_accept_task_tls_server_handshake_failed_params()
        if not params:
            return []
        else:
            return [
                (
                    params.node_addr,
                    params.peer_addr,
                )
            ]


class ReplicaDivergedEvent(ReplicaEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(name="replica_diverged", doc=doc, crate="ic_state_manager", module="ic_state_manager")

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.state_manager_replica_diverged_params()
        if not params:
            return []
        else:
            return [(self.doc.get_node_id(), self.doc.get_subnet_id(), str(params.height))]


class CupShareProposedEvent(ReplicaEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(name="CUP_share_proposed", doc=doc, crate="ic_state_manager", module="catchup_package_maker")

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_catchup_package_share_params()
        if not params:
            return []
        else:
            return [(self.doc.get_node_id(), self.doc.get_subnet_id(), str(params.height))]


class FinalizedEvent(ReplicaEvent):
    def __init__(self, doc: EsDoc):
        super().__init__(name="finalized", doc=doc, crate="ic_consensus", module="batch_delivery")

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_batch_delivery_consensus_params()
        if not params:
            return []
        else:
            return [
                (
                    self.doc.get_node_id(),
                    self.doc.get_subnet_id(),
                    str(params.height),
                    params.hash,
                    params.replica_version,
                )
            ]


class GenericLogEvent(Event):
    doc: EsDoc

    def __init__(self, doc: EsDoc):
        super().__init__(name="log", doc=doc)

    def compile_params(self) -> Iterable[Tuple[str, ...]]:
        params = self.doc.get_generic_params()
        if not params:
            return []
        else:
            return [
                (
                    self.doc.host_id(),
                    params.node_id,
                    params.subnet_id,
                    params.component_id,
                    params.level,
                    params.message,
                )
            ]
