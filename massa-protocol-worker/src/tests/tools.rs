use crate::start_protocol_controller;
use futures::Future;
use massa_consensus_exports::test_exports::{ConsensusEventReceiver, MockConsensusController};
use massa_models::{
    block::{BlockId, WrappedBlock},
    node::NodeId,
    operation::WrappedOperation,
    prehash::PreHashSet,
};
use massa_network_exports::BlockInfoReply;
use massa_pool_exports::test_exports::{MockPoolController, PoolEventReceiver};
use massa_protocol_exports::{
    tests::mock_network_controller::MockNetworkController, ProtocolCommandSender, ProtocolConfig,
    ProtocolManager, ProtocolReceivers, ProtocolSenders,
};
use massa_storage::Storage;
use tokio::sync::{broadcast, mpsc};

pub async fn protocol_test<F, V>(protocol_config: &ProtocolConfig, test: F)
where
    F: FnOnce(
        MockNetworkController,
        ProtocolCommandSender,
        ProtocolManager,
        ConsensusEventReceiver,
        PoolEventReceiver,
    ) -> V,
    V: Future<
        Output = (
            MockNetworkController,
            ProtocolCommandSender,
            ProtocolManager,
            ConsensusEventReceiver,
            PoolEventReceiver,
        ),
    >,
{
    let (network_controller, network_command_sender, network_event_receiver) =
        MockNetworkController::new();

    let (pool_controller, pool_event_receiver) = MockPoolController::new_with_receiver();
    let (consensus_controller, consensus_event_receiver) =
        MockConsensusController::new_with_receiver();
    // start protocol controller
    let (protocol_command_sender, protocol_command_receiver) =
        mpsc::channel(protocol_config.controller_channel_size);
    let operation_sender = broadcast::channel(protocol_config.broadcast_operations_capacity).0;
    let protocol_receivers = ProtocolReceivers {
        network_event_receiver,
        protocol_command_receiver,
    };
    let protocol_senders = ProtocolSenders {
        network_command_sender,
        operation_sender,
    };
    // start protocol controller
    let protocol_manager: ProtocolManager = start_protocol_controller(
        *protocol_config,
        protocol_receivers,
        protocol_senders,
        consensus_controller,
        pool_controller,
        Storage::create_root(),
    )
    .await
    .expect("could not start protocol controller");

    let protocol_command_sender = ProtocolCommandSender(protocol_command_sender);
    let (
        _network_controller,
        _protocol_command_sender,
        protocol_manager,
        _consensus_event_receiver,
        _pool_event_receiver,
    ) = test(
        network_controller,
        protocol_command_sender,
        protocol_manager,
        consensus_event_receiver,
        pool_event_receiver,
    )
    .await;

    protocol_manager
        .stop()
        .await
        .expect("Failed to shutdown protocol.");
}

pub async fn protocol_test_with_storage<F, V>(protocol_config: &ProtocolConfig, test: F)
where
    F: FnOnce(
        MockNetworkController,
        ProtocolCommandSender,
        ProtocolManager,
        ConsensusEventReceiver,
        PoolEventReceiver,
        Storage,
    ) -> V,
    V: Future<
        Output = (
            MockNetworkController,
            ProtocolCommandSender,
            ProtocolManager,
            ConsensusEventReceiver,
            PoolEventReceiver,
        ),
    >,
{
    let (network_controller, network_command_sender, network_event_receiver) =
        MockNetworkController::new();
    let (pool_controller, mock_pool_receiver) = MockPoolController::new_with_receiver();
    let (consensus_controller, mock_consensus_receiver) =
        MockConsensusController::new_with_receiver();
    let storage = Storage::create_root();
    // start protocol controller
    let (protocol_command_sender, protocol_command_receiver) =
        mpsc::channel(protocol_config.controller_channel_size);

    let protocol_senders = ProtocolSenders {
        network_command_sender: network_command_sender.clone(),
        operation_sender: broadcast::channel(protocol_config.broadcast_operations_capacity).0,
    };

    let protocol_receivers = ProtocolReceivers {
        network_event_receiver,
        protocol_command_receiver,
    };

    let protocol_manager = start_protocol_controller(
        *protocol_config,
        protocol_receivers,
        protocol_senders,
        consensus_controller,
        pool_controller,
        storage.clone(),
    )
    .await
    .expect("could not start protocol controller");

    let protocol_command_sender = ProtocolCommandSender(protocol_command_sender);
    let (
        _network_controller,
        _protocol_command_sender,
        protocol_manager,
        _consensus_event_receiver,
        _protocol_pool_event_receiver,
    ) = test(
        network_controller,
        protocol_command_sender,
        protocol_manager,
        mock_consensus_receiver,
        mock_pool_receiver,
        storage,
    )
    .await;

    protocol_manager
        .stop()
        .await
        .expect("Failed to shutdown protocol.");
}

/// send a block and assert it has been propagate (or not)
pub async fn send_and_propagate_block(
    network_controller: &mut MockNetworkController,
    block: WrappedBlock,
    source_node_id: NodeId,
    protocol_command_sender: &mut ProtocolCommandSender,
    operations: Vec<WrappedOperation>,
) {
    network_controller
        .send_header(source_node_id, block.content.header.clone())
        .await;

    let mut protocol_sender = protocol_command_sender.clone();
    tokio::task::spawn_blocking(move || {
        protocol_sender
            .send_wishlist_delta(
                vec![(block.id, Some(block.content.header.clone()))]
                    .into_iter()
                    .collect(),
                PreHashSet::<BlockId>::default(),
            )
            .unwrap();
    })
    .await
    .unwrap();

    // Send block info to protocol.
    let info = vec![(
        block.id,
        BlockInfoReply::Info(block.content.operations.clone()),
    )];
    network_controller
        .send_block_info(source_node_id, info)
        .await;

    // Send full ops.
    let info = vec![(block.id, BlockInfoReply::Operations(operations))];
    network_controller
        .send_block_info(source_node_id, info)
        .await;
}
