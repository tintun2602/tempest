# Idé: airdrop-farming på autopilot (parkert 2026-11-10)

Status: **idé, ikke bygget.** Tintun 10.11: «legg det som en idé i Tempest bare for nå».

Kort: en egen bot med egen lommebok (ikke Tintuns MetaMask) gjør ekte, små transaksjoner på billige kjeder
(Base først), for å kvalifisere for mulige airdrops. Faste regler, ingen språkmodell i beslutninger, én lommebok
(ingen sybil), gasstak, og hver transaksjon logges for skatt.

- Plan og kildesjekk: playbook `agents/trading/airdrop-plan.md` (Gamma, 10.11).
- Funn: få gode mål nå. Variational forbyr bots; Ostium-poeng er ikke bekreftet; Base har ikke lovet token.
- Kostnad hvis den startes: ca. 0,5–2 USD/mnd på Base. Budsjett 100–500 USD, regnes som tapt.
- Delta-nøytral sikring på Binance (via Tempest) blir først aktuelt med mer kapital.
- Kjøres i egen container og egen Linux-bruker på Hetzner, adskilt fra Tempests Binance-nøkkel.

Start: Tintun sier beløp og «kjør».
