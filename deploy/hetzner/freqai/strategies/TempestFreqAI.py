"""FreqAI live strategy for the Tempest account: Freqtrade's FreqaiExampleStrategy
on 1h candles, 3x, with stops and targets scaled for leverage.

The example itself warns it is a showcase, not tuned for production. Changes
here are only the ones our lab showed matter: a slower timeframe (the 3m
default pays fees on nearly every candle) and limits sized for leverage.
"""

from datetime import datetime

from freqtrade.templates.FreqaiExampleStrategy import FreqaiExampleStrategy

LEVERAGE = 3.0


class TempestFreqAI(FreqaiExampleStrategy):
    timeframe = "1h"
    # Freqtrade measures stoploss and ROI on the leveraged stake, so at 3x
    # -0.15 is a 5% price move against the trade and 0.30 a 10% move for it.
    stoploss = -0.15
    # Take 10% (price) at any time; after 24 hours, the horizon the model
    # predicts over, exit at whatever the trade shows.
    minimal_roi = {"0": 0.30, "1440": -1}

    def leverage(
        self,
        pair: str,
        current_time: datetime,
        current_rate: float,
        proposed_leverage: float,
        max_leverage: float,
        entry_tag: str | None,
        side: str,
        **kwargs,
    ) -> float:
        return min(LEVERAGE, max_leverage)
