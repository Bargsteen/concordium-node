{-# LANGUAGE DataKinds #-}
module SchedulerTests.Helpers where

import Concordium.Scheduler.Types
import qualified Concordium.Cost as Cost
import qualified Concordium.Scheduler.Types as Types

getResults :: [(a, TransactionSummary)] -> [(a, ValidResult)]
getResults = map (\(x, r) -> (x, tsResult r))

-- | The cost for processing a simple transfer (account to account)
-- with one signature in the transaction.
--
-- * @SPEC: <$DOCS/Transactions#transaction-cost-header-simple-transfer>
simpleTransferCost :: Energy
simpleTransferCost = Cost.baseCost (Types.transactionHeaderSize + 41) 1 + Cost.simpleTransferCost

-- |Protocol version
type PV = 'Types.P1
