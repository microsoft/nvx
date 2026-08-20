#!/usr/bin/env python3
import pandas as pd, numpy as np


def work():
    df = pd.DataFrame({"x": np.arange(5), "y": np.arange(5) ** 2})
    return df.sum().to_dict()


print(work())
